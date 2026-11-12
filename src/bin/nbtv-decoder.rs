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
#[command(about = "Receive classic 32-line monochrome NBTV video from a mono JACK port")]
struct Args {
    #[arg(long, default_value = "nbtv-decoder")]
    name: String,
    /// Source JACK output port.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect: Option<String>,
    /// Save the latest complete frame as a PNG (overwritten at video rate).
    #[arg(long)]
    output: Option<PathBuf>,
    /// Synchronization threshold in JACK signal units; sync must be below it.
    #[arg(long, default_value_t = nbtv::DEFAULT_THRESHOLD, allow_hyphen_values = true, value_parser = threshold)]
    threshold: f32,
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
}

impl Reception {
    fn new() -> Self {
        Self {
            pixels: vec![0; nbtv::FRAME_PIXELS],
            frames: 0,
            locked: false,
        }
    }
}

struct Shared {
    state: Arc<RunState>,
    reception: Mutex<Reception>,
    reset: AtomicBool,
    threshold: AtomicU32,
    dropped: AtomicUsize,
    finished: AtomicBool,
}

fn receive(
    mut consumer: rtrb::Consumer<f32>,
    shared: &Shared,
    sample_rate: u32,
    output: Option<PathBuf>,
) -> Result<()> {
    let mut decoder = nbtv::Decoder::new(sample_rate)?;
    let mut until_status = 0;
    while shared.state.is_running() {
        if shared.reset.load(Ordering::Acquire) {
            let buffered = consumer.slots();
            for _ in 0..buffered {
                let _ = consumer.pop();
            }
            decoder = nbtv::Decoder::new(sample_rate)?;
            *shared
                .reception
                .lock()
                .map_err(|_| anyhow!("Reception lock poisoned"))? = Reception::new();
            shared.reset.store(false, Ordering::Release);
        }
        let sample = match consumer.pop() {
            Ok(sample) => sample,
            Err(_) => {
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        };
        let threshold = f32::from_bits(shared.threshold.load(Ordering::Relaxed));
        if let Some(pixels) = decoder.push(sample, threshold) {
            if let Some(path) = &output {
                image::save_buffer(
                    path,
                    &pixels,
                    nbtv::WIDTH as u32,
                    nbtv::HEIGHT as u32,
                    image::ColorType::L8,
                )
                .with_context(|| format!("Cannot save frame to {}", path.display()))?;
            }
            let mut reception = shared
                .reception
                .lock()
                .map_err(|_| anyhow!("Reception lock poisoned"))?;
            reception.pixels = pixels;
            reception.frames += 1;
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
}

impl eframe::App for DecoderApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if !self.shared.state.is_running()
            || self.shared.finished.load(Ordering::Acquire)
            || ctx.input(|input| input.key_pressed(egui::Key::Escape))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let (pixels, frames, locked) = match self.shared.reception.lock() {
            Ok(reception) => (reception.pixels.clone(), reception.frames, reception.locked),
            Err(_) => {
                eprintln!("Reception lock poisoned");
                self.shared.state.stop();
                return;
            }
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "NBTV Club 32-line - 12.5 fps - {}",
            if locked {
                "sync locked"
            } else {
                "searching for sync"
            }
        )));
        let image = egui::ColorImage::from_gray([nbtv::WIDTH, nbtv::HEIGHT], &pixels);
        if let Some(texture) = &mut self.texture {
            texture.set(image, egui::TextureOptions::NEAREST);
        } else {
            self.texture = Some(ctx.load_texture("nbtv", image, egui::TextureOptions::NEAREST));
        }
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
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
            ui.small("NBTV has no protocol header. Sync is embedded in every frame.");
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
    let (mut producer, consumer) = rtrb::RingBuffer::new(2 * sample_rate as usize);
    let shared = Arc::new(Shared {
        state: Arc::clone(&state),
        reception: Mutex::new(Reception::new()),
        reset: AtomicBool::new(false),
        threshold: AtomicU32::new(args.threshold.to_bits()),
        dropped: AtomicUsize::new(0),
        finished: AtomicBool::new(false),
    });
    let callback_shared = Arc::clone(&shared);
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, scope: &jack::ProcessScope| {
            let mut missed = 0;
            for &sample in input.as_slice(scope) {
                if producer.push(sample).is_err() {
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
    eprintln!("Input: {port_name} ({sample_rate} Hz, NBTV Club 32-line). Esc/Ctrl-C exits.");
    if let Some(source) = &args.connect {
        active
            .as_client()
            .connect_ports_by_name(source, &port_name)
            .with_context(|| format!("Cannot connect from {source}"))?;
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
    fn live_receiver_resets_without_input_then_receives_a_new_clip() {
        let state = RunState::new();
        let shared = Arc::new(Shared {
            state: Arc::clone(&state),
            reception: Mutex::new(Reception::new()),
            reset: AtomicBool::new(false),
            threshold: AtomicU32::new(nbtv::DEFAULT_THRESHOLD.to_bits()),
            dropped: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
        });
        let (mut producer, consumer) = rtrb::RingBuffer::new(96_000);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || receive(consumer, &worker_shared, 48_000, None));
        shared.reset.store(true, Ordering::Release);
        let idle_reset = wait_until(|| !shared.reset.load(Ordering::Acquire));
        let feed = |producer: &mut rtrb::Producer<f32>, value| {
            let clip = nbtv::Clip::new(vec![value; nbtv::FRAME_PIXELS * 5]).unwrap();
            for position in 0..clip.samples(48_000) {
                producer.push(clip.sample(position, 48_000)).unwrap();
            }
        };
        feed(&mut producer, 200);
        let first = wait_until(|| shared.reception.lock().unwrap().frames >= 3);
        shared.reset.store(true, Ordering::Release);
        let reset = wait_until(|| !shared.reset.load(Ordering::Acquire));
        let cleared = {
            let reception = shared.reception.lock().unwrap();
            reception.frames == 0 && reception.pixels.iter().all(|pixel| *pixel == 0)
        };
        feed(&mut producer, 50);
        let second = wait_until(|| shared.reception.lock().unwrap().frames >= 3);
        state.stop();
        worker.join().unwrap().unwrap();
        assert!(idle_reset && first && reset && cleared && second);
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
