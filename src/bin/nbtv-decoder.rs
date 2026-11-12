use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use eframe::egui;
use sstv_jack::{Notifications, POLL_INTERVAL, RunState, nbtv};

#[derive(Parser)]
#[command(about = "Receive 32-line monochrome NBTV video over JACK")]
struct Args {
    #[arg(long, default_value = "nbtv-decoder")]
    name: String,
    /// Source JACK output port.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect: Option<String>,
    /// Signal format; choose the same mode in the encoder.
    #[arg(long, value_enum, default_value_t = nbtv::Mode::Classic)]
    mode: nbtv::Mode,
    /// Clean sync source for separate-sync mode.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect_sync: Option<String>,
    /// Delay clean sync to match picture-path latency (separate-sync only).
    #[arg(long, default_value_t = 0.0, value_parser = sync_delay_ms)]
    sync_delay_ms: f64,
    /// Save frames as a PNG; keep the latest pending frame if storage falls behind.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Synchronization threshold in JACK signal units; sync must be below it.
    #[arg(long, default_value_t = nbtv::DEFAULT_THRESHOLD, allow_hyphen_values = true, value_parser = threshold)]
    threshold: f32,
}

fn sync_delay_ms(value: &str) -> std::result::Result<f64, String> {
    let value: f64 = value
        .parse()
        .map_err(|_| "Sync delay must be a number".to_string())?;
    if value.is_finite() && (0.0..=2000.0).contains(&value) {
        Ok(value)
    } else {
        Err("Sync delay must be between 0 and 2000 milliseconds".into())
    }
}

fn threshold(value: &str) -> std::result::Result<f32, String> {
    let value: f32 = value
        .parse()
        .map_err(|_| "Threshold must be a number".to_string())?;
    if value.is_finite() && (-0.8..=-0.01).contains(&value) {
        Ok(value)
    } else {
        Err("Threshold must be between -0.8 and -0.01".into())
    }
}

struct Reception {
    pixels: Vec<u8>,
    frames: usize,
    locked: bool,
    revision: usize,
}

impl Reception {
    fn new() -> Self {
        Self {
            pixels: vec![0; nbtv::FRAME_PIXELS],
            frames: 0,
            locked: false,
            revision: 0,
        }
    }
}

struct Shared {
    state: Arc<RunState>,
    reception: Mutex<Reception>,
    reset: AtomicBool,
    threshold: AtomicU32,
    separate: AtomicBool,
    delay_samples: AtomicUsize,
    dropped: AtomicUsize,
    finished: AtomicBool,
    skipped_saves: Arc<AtomicUsize>,
}

fn receive(
    consumer: rtrb::Consumer<(f32, f32)>,
    shared: &Shared,
    sample_rate: u32,
    output: Option<PathBuf>,
) -> Result<()> {
    let mut saver =
        output.map(|path| nbtv::saving::FrameSaver::new(path, Arc::clone(&shared.skipped_saves)));
    let result = receive_frames(consumer, shared, sample_rate, &mut saver);
    let saving = if let Some(saver) = &mut saver {
        saver.finish()
    } else {
        Ok(())
    };
    if let Err(error) = saving {
        if result.is_err() {
            eprintln!("{error:#}");
        } else {
            return Err(error);
        }
    }
    result
}

fn receive_frames(
    mut consumer: rtrb::Consumer<(f32, f32)>,
    shared: &Shared,
    sample_rate: u32,
    saver: &mut Option<nbtv::saving::FrameSaver>,
) -> Result<()> {
    let mut decoder = nbtv::Decoder::new(sample_rate)?;
    let mut separate = shared.separate.load(Ordering::Acquire);
    let mut delay = nbtv::SyncDelay::new(if separate {
        shared.delay_samples.load(Ordering::Relaxed)
    } else {
        0
    });
    let mut until_status = 0;
    while shared.state.is_running() {
        if shared.reset.load(Ordering::Acquire) {
            let buffered = consumer.slots();
            for _ in 0..buffered {
                let _ = consumer.pop();
            }
            decoder = nbtv::Decoder::new(sample_rate)?;
            separate = shared.separate.load(Ordering::Acquire);
            delay = nbtv::SyncDelay::new(if separate {
                shared.delay_samples.load(Ordering::Relaxed)
            } else {
                0
            });
            let mut reception = shared
                .reception
                .lock()
                .map_err(|_| anyhow!("Reception lock poisoned"))?;
            let revision = reception.revision.wrapping_add(1);
            *reception = Reception::new();
            reception.revision = revision;
            drop(reception);
            shared.reset.store(false, Ordering::Release);
        }
        let (picture, sync) = match consumer.pop() {
            Ok(sample) => sample,
            Err(_) => {
                if let Some(saver) = saver {
                    saver.check()?;
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        let threshold = f32::from_bits(shared.threshold.load(Ordering::Relaxed));
        let sync = if separate { delay.push(sync) } else { picture };
        if let Some(pixels) = decoder.push_channels(picture, sync, threshold) {
            if let Some(saver) = saver {
                saver.submit(&pixels)?;
            }
            let mut reception = shared
                .reception
                .lock()
                .map_err(|_| anyhow!("Reception lock poisoned"))?;
            reception.pixels = pixels;
            reception.frames += 1;
            reception.revision = reception.revision.wrapping_add(1);
        }
        if until_status == 0 {
            shared
                .reception
                .lock()
                .map_err(|_| anyhow!("Reception lock poisoned"))?
                .locked = decoder.locked();
            until_status = 512;
        }
        until_status -= 1;
    }
    Ok(())
}

struct DecoderApp {
    shared: Arc<Shared>,
    texture: Option<egui::TextureHandle>,
    last_dropped: usize,
    last_xruns: usize,
    sample_rate: u32,
    uploaded_revision: Option<usize>,
    snapshot: Vec<u8>,
    last_title: String,
    last_skipped_saves: usize,
    color_image: Arc<egui::ColorImage>,
}

impl DecoderApp {
    fn refresh_texture(&mut self, ctx: &egui::Context) -> Result<usize> {
        let (changed, frames, locked) = {
            let reception = self
                .shared
                .reception
                .lock()
                .map_err(|_| anyhow!("Reception lock poisoned"))?;
            let changed = self.uploaded_revision != Some(reception.revision);
            if changed {
                self.snapshot.clone_from(&reception.pixels);
                self.uploaded_revision = Some(reception.revision);
            }
            (changed, reception.frames, reception.locked)
        };
        let title = format!(
            "NBTV {} - 32-line - 12.5 fps - {}",
            nbtv::Mode::from_separate(self.shared.separate.load(Ordering::Acquire)).label(),
            if locked {
                "sync locked"
            } else {
                "searching for sync"
            }
        );
        if self.last_title != title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
        if changed {
            let image = Arc::make_mut(&mut self.color_image);
            image
                .pixels
                .resize(self.snapshot.len(), egui::Color32::BLACK);
            for (target, value) in image.pixels.iter_mut().zip(&self.snapshot) {
                *target = egui::Color32::from_gray(*value);
            }
            if let Some(texture) = &mut self.texture {
                texture.set(Arc::clone(&self.color_image), egui::TextureOptions::NEAREST);
            } else {
                self.texture = Some(ctx.load_texture(
                    "nbtv",
                    Arc::clone(&self.color_image),
                    egui::TextureOptions::NEAREST,
                ));
            }
        }
        Ok(frames)
    }
}

impl eframe::App for DecoderApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if !self.shared.state.is_running()
            || self.shared.finished.load(Ordering::Acquire)
            || ctx.input(|input| input.key_pressed(egui::Key::Escape))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let frames = match self.refresh_texture(ctx) {
            Ok(frames) => frames,
            Err(error) => {
                eprintln!("{error:#}");
                self.shared.state.stop();
                return;
            }
        };
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            let mut mode = nbtv::Mode::from_separate(self.shared.separate.load(Ordering::Acquire));
            let previous = mode;
            egui::ComboBox::from_label("Mode")
                .selected_text(mode.label())
                .show_ui(ui, |ui| {
                    for candidate in [nbtv::Mode::Classic, nbtv::Mode::SeparateSync] {
                        ui.selectable_value(&mut mode, candidate, candidate.label());
                    }
                });
            if mode != previous {
                self.shared
                    .separate
                    .store(mode == nbtv::Mode::SeparateSync, Ordering::Release);
                self.shared.reset.store(true, Ordering::Release);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.shared.reset.load(Ordering::Acquire),
                        egui::Button::new("Reset"),
                    )
                    .clicked()
                {
                    self.shared.reset.store(true, Ordering::Release);
                }
                ui.label(format!("Received frames: {frames}"));
            });
            let mut threshold = f32::from_bits(self.shared.threshold.load(Ordering::Relaxed));
            if ui
                .add(egui::Slider::new(&mut threshold, -0.8..=-0.01).text("Sync threshold"))
                .changed()
            {
                self.shared
                    .threshold
                    .store(threshold.to_bits(), Ordering::Relaxed);
                self.shared.reset.store(true, Ordering::Release);
            }
            ui.add_enabled_ui(mode == nbtv::Mode::SeparateSync, |ui| {
                let samples = self.shared.delay_samples.load(Ordering::Relaxed);
                let mut delay_ms = samples as f64 * 1000.0 / f64::from(self.sample_rate);
                ui.horizontal(|ui| {
                    ui.label("Sync delay");
                    if ui
                        .add(
                            egui::DragValue::new(&mut delay_ms)
                                .range(0.0..=2000.0)
                                .speed(0.01)
                                .max_decimals(3)
                                .suffix(" ms"),
                        )
                        .changed()
                    {
                        self.shared.delay_samples.store(
                            (delay_ms * f64::from(self.sample_rate) / 1000.0).round() as usize,
                            Ordering::Relaxed,
                        );
                        self.shared.reset.store(true, Ordering::Release);
                    }
                });
            });
            ui.small(if mode == nbtv::Mode::Classic {
                "Sync is embedded in the picture input."
            } else {
                "Route picture through effects; feed clean sync into the sync input."
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.texture {
                ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(ui.available_size())
                        .maintain_aspect_ratio(true),
                );
            }
        });
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        if dropped != self.last_dropped {
            eprintln!("Warning: NBTV input overflow, {dropped} samples lost.");
            self.last_dropped = dropped;
        }
        let skipped = self.shared.skipped_saves.load(Ordering::Relaxed);
        if skipped != self.last_skipped_saves {
            eprintln!(
                "Warning: frame saving is behind; {skipped} superseded frames were not saved."
            );
            self.last_skipped_saves = skipped;
        }
        let xruns = self.shared.state.xruns.load(Ordering::Relaxed);
        if xruns != self.last_xruns {
            eprintln!("Warning: JACK xrun count {xruns}; NBTV synchronization may be damaged.");
            self.last_xruns = xruns;
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let state = RunState::new();
    state.install_interrupt_handler()?;
    let (client, _) = jack::Client::new(&args.name, jack::ClientOptions::NO_START_SERVER)
        .context("Cannot connect to JACK; start a server or use pw-jack")?;
    let sample_rate = client.sample_rate();
    nbtv::Decoder::new(sample_rate)?;
    let input = client.register_port("input", jack::AudioIn::default())?;
    let port_name = input.name()?;
    let sync_input = client.register_port("sync", jack::AudioIn::default())?;
    let sync_port_name = sync_input.name()?;
    let (mut producer, consumer) = rtrb::RingBuffer::new(2 * sample_rate as usize);
    let shared = Arc::new(Shared {
        state: Arc::clone(&state),
        reception: Mutex::new(Reception::new()),
        reset: AtomicBool::new(false),
        threshold: AtomicU32::new(args.threshold.to_bits()),
        separate: AtomicBool::new(args.mode == nbtv::Mode::SeparateSync),
        delay_samples: AtomicUsize::new(
            (args.sync_delay_ms * f64::from(sample_rate) / 1000.0).round() as usize,
        ),
        dropped: AtomicUsize::new(0),
        finished: AtomicBool::new(false),
        skipped_saves: Arc::new(AtomicUsize::new(0)),
    });
    let callback_shared = Arc::clone(&shared);
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, scope: &jack::ProcessScope| {
            let mut missed = 0;
            for (&picture, &sync) in input.as_slice(scope).iter().zip(sync_input.as_slice(scope)) {
                if producer.push((picture, sync)).is_err() {
                    missed += 1;
                }
            }
            if missed > 0 {
                callback_shared.dropped.fetch_add(missed, Ordering::Relaxed);
            }
            jack::Control::Continue
        },
    );
    let active = client.activate_async(
        Notifications {
            state: Arc::clone(&state),
            sample_rate,
        },
        process,
    )?;
    eprintln!(
        "Input: {port_name} | Sync: {sync_port_name} ({sample_rate} Hz, {}). Esc/Ctrl-C exits.",
        args.mode.label()
    );
    if let Some(source) = &args.connect {
        active
            .as_client()
            .connect_ports_by_name(source, &port_name)
            .with_context(|| format!("Cannot connect from {source}"))?;
    }
    if let Some(source) = &args.connect_sync {
        active
            .as_client()
            .connect_ports_by_name(source, &sync_port_name)
            .with_context(|| format!("Cannot connect sync from {source}"))?;
    }
    let worker_shared = Arc::clone(&shared);
    let worker = std::thread::spawn(move || {
        let result = receive(consumer, &worker_shared, sample_rate, args.output);
        worker_shared.finished.store(true, Ordering::Release);
        result
    });
    let app = DecoderApp {
        shared,
        texture: None,
        last_dropped: 0,
        last_xruns: 0,
        sample_rate,
        uploaded_revision: None,
        snapshot: Vec::new(),
        last_title: String::new(),
        last_skipped_saves: 0,
        color_image: Arc::new(egui::ColorImage::new(
            [nbtv::WIDTH, nbtv::HEIGHT],
            vec![egui::Color32::BLACK; nbtv::FRAME_PIXELS],
        )),
    };
    let result = eframe::run_native(
        "NBTV decoder",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([480.0, 740.0]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(app))),
    )
    .map_err(|error| anyhow!("Cannot run NBTV decoder GUI: {error}"));
    state.stop();
    let worker_result = worker
        .join()
        .map_err(|_| anyhow!("NBTV decoder worker panicked"))?;
    let deactivation = active.deactivate().context("Cannot deactivate JACK client");
    state.check()?;
    result?;
    deactivation?;
    worker_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_uploads_only_on_new_frames_or_reset() {
        let shared = Arc::new(Shared {
            state: RunState::new(),
            reception: Mutex::new(Reception::new()),
            reset: AtomicBool::new(false),
            threshold: AtomicU32::new(nbtv::DEFAULT_THRESHOLD.to_bits()),
            separate: AtomicBool::new(false),
            delay_samples: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            skipped_saves: Arc::new(AtomicUsize::new(0)),
        });
        let mut app = DecoderApp {
            shared: Arc::clone(&shared),
            texture: None,
            last_dropped: 0,
            last_xruns: 0,
            sample_rate: 48_000,
            uploaded_revision: None,
            snapshot: Vec::new(),
            last_title: String::new(),
            last_skipped_saves: 0,
            color_image: Arc::new(egui::ColorImage::new(
                [nbtv::WIDTH, nbtv::HEIGHT],
                vec![egui::Color32::BLACK; nbtv::FRAME_PIXELS],
            )),
        };
        let ctx = egui::Context::default();
        let first = ctx.run(Default::default(), |ctx| {
            app.refresh_texture(ctx).unwrap();
        });
        let texture = app.texture.as_ref().unwrap().id();
        assert!(
            first
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture)
        );
        shared.reception.lock().unwrap().locked = true;
        let idle = ctx.run(Default::default(), |ctx| {
            app.refresh_texture(ctx).unwrap();
        });
        assert!(!idle.textures_delta.set.iter().any(|(id, _)| *id == texture));
        {
            let mut frame = shared.reception.lock().unwrap();
            frame.pixels[0] = 255;
            frame.frames += 1;
            frame.revision += 1;
        }
        let changed = ctx.run(Default::default(), |ctx| {
            app.refresh_texture(ctx).unwrap();
        });
        assert!(
            changed
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture)
        );
        assert_eq!(app.color_image.pixels[0], egui::Color32::WHITE);
        {
            let mut frame = shared.reception.lock().unwrap();
            let revision = frame.revision + 1;
            *frame = Reception::new();
            frame.revision = revision;
        }
        let reset = ctx.run(Default::default(), |ctx| {
            app.refresh_texture(ctx).unwrap();
        });
        assert!(
            reset
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture)
        );
        assert_eq!(app.color_image.pixels[0], egui::Color32::BLACK);
    }

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        false
    }

    #[test]
    fn live_receiver_resets_without_input_and_switches_signal_modes() {
        let state = RunState::new();
        let shared = Arc::new(Shared {
            state: Arc::clone(&state),
            reception: Mutex::new(Reception::new()),
            reset: AtomicBool::new(false),
            threshold: AtomicU32::new(nbtv::DEFAULT_THRESHOLD.to_bits()),
            separate: AtomicBool::new(false),
            delay_samples: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            skipped_saves: Arc::new(AtomicUsize::new(0)),
        });
        let (mut producer, consumer) = rtrb::RingBuffer::new(96_000);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || receive(consumer, &worker_shared, 48_000, None));
        shared.reset.store(true, Ordering::Release);
        let idle_reset = wait_until(|| !shared.reset.load(Ordering::Acquire));
        let feed = |producer: &mut rtrb::Producer<(f32, f32)>, value, separate| {
            let clip = nbtv::Clip::new(vec![value; nbtv::FRAME_PIXELS * 5]).unwrap();
            for position in 0..clip.samples(48_000) {
                let picture = clip.sample(position, 48_000);
                producer
                    .push(if separate {
                        (
                            picture.max(0.0) * 0.1 - 0.8,
                            nbtv::sync_sample(position, 48_000),
                        )
                    } else {
                        (picture, 0.0)
                    })
                    .unwrap();
            }
        };
        feed(&mut producer, 200, false);
        let first = wait_until(|| shared.reception.lock().unwrap().frames >= 3);
        shared.separate.store(true, Ordering::Release);
        shared.delay_samples.store(737, Ordering::Relaxed);
        shared.reset.store(true, Ordering::Release);
        let reset = wait_until(|| !shared.reset.load(Ordering::Acquire));
        let cleared = {
            let reception = shared.reception.lock().unwrap();
            reception.frames == 0 && reception.pixels.iter().all(|pixel| *pixel == 0)
        };
        feed(&mut producer, 50, true);
        let second = wait_until(|| shared.reception.lock().unwrap().frames >= 3);
        let distorted = shared
            .reception
            .lock()
            .unwrap()
            .pixels
            .iter()
            .all(|pixel| *pixel == 0);
        shared.separate.store(false, Ordering::Release);
        shared.reset.store(true, Ordering::Release);
        let classic_reset = wait_until(|| !shared.reset.load(Ordering::Acquire));
        feed(&mut producer, 50, false);
        let third = wait_until(|| shared.reception.lock().unwrap().frames >= 3);
        state.stop();
        worker.join().unwrap().unwrap();
        assert!(
            idle_reset
                && first
                && reset
                && cleared
                && second
                && distorted
                && classic_reset
                && third
        );
        assert!(
            shared
                .reception
                .lock()
                .unwrap()
                .pixels
                .iter()
                .all(|pixel| pixel.abs_diff(50) <= 1)
        );
    }
}
