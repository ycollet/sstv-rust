use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use eframe::egui;
use sstv::Decoder;
use sstv_jack::{LiveSamples, Notifications, RunState, pcm_sample};

#[derive(Parser)]
#[command(about = "Receive SSTV from a mono JACK input and display image rows as they arrive")]
struct Args {
    /// JACK client name.
    #[arg(long, default_value = "sstv-decoder")]
    name: String,
    /// Source JACK output port; otherwise connect manually.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect: Option<String>,
    /// Save each complete reception to this image path (overwrites the previous image).
    #[arg(long, value_name = "IMAGE.png")]
    output: Option<PathBuf>,
}

struct Display {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    title: String,
    revision: usize,
}

impl Display {
    fn waiting() -> Self {
        Self {
            width: 320,
            height: 240,
            pixels: vec![0; 320 * 240],
            title: "SSTV - waiting for a transmission".into(),
            revision: 0,
        }
    }
}

fn receive(
    samples: impl Iterator<Item = i16>,
    sample_rate: u32,
    state: &RunState,
    display: &Mutex<Display>,
    output: Option<PathBuf>,
    reset: &AtomicBool,
) -> Result<()> {
    let mut samples = samples;
    while state.is_running() && !reset.load(Ordering::Acquire) {
        let input = samples
            .by_ref()
            .take_while(|_| !reset.load(Ordering::Acquire));
        let Some(rows) = Decoder::new(input, sample_rate).rows() else {
            break;
        };
        if reset.load(Ordering::Acquire) {
            break;
        }
        let mode = rows.mode();
        let (width, height) = mode.resolution();
        let (width, height) = (width as usize, height as usize);
        eprintln!("Receiving {mode:?} ({width}x{height}) SSTV image.");
        {
            let mut frame = display
                .lock()
                .map_err(|_| anyhow!("Display lock poisoned"))?;
            frame.width = width;
            frame.height = height;
            frame.pixels = vec![0; width * height];
            frame.revision = frame.revision.wrapping_add(1);
            frame.title = format!("SSTV - {mode:?} - receiving {width}x{height}");
        }
        let mut received = 0;
        for row in rows {
            if reset.load(Ordering::Acquire) {
                break;
            }
            let mut frame = display
                .lock()
                .map_err(|_| anyhow!("Display lock poisoned"))?;
            let start = row.index() * width;
            for (target, pixel) in frame.pixels[start..start + width]
                .iter_mut()
                .zip(row.pixels())
            {
                *target = (u32::from(pixel.red()) << 16)
                    | (u32::from(pixel.green()) << 8)
                    | u32::from(pixel.blue());
            }
            received += 1;
            frame.revision = frame.revision.wrapping_add(1);
            frame.title = format!("SSTV - {mode:?} - {width}x{height} - row {received}/{height}");
        }
        if !state.is_running() || reset.load(Ordering::Acquire) {
            break;
        }
        if received != height {
            eprintln!("Warning: incomplete image ({received}/{height} rows). Listening again.");
            continue;
        }
        let image = {
            let mut frame = display
                .lock()
                .map_err(|_| anyhow!("Display lock poisoned"))?;
            frame.title =
                format!("SSTV - {mode:?} - {width}x{height} complete - listening for next image");
            output.as_ref().map(|_| {
                image::RgbImage::from_fn(width as u32, height as u32, |x, y| {
                    let pixel = frame.pixels[y as usize * width + x as usize];
                    image::Rgb([(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8])
                })
            })
        };
        if let (Some(path), Some(rgb)) = (&output, image) {
            rgb.save(path)
                .with_context(|| format!("Cannot save image to {}", path.display()))?;
            eprintln!("Saved {}", path.display());
        }
        eprintln!("Image complete. Listening for the next transmission.");
    }
    Ok(())
}

fn receive_live(
    mut samples: LiveSamples,
    sample_rate: u32,
    state: &RunState,
    display: &Mutex<Display>,
    output: Option<PathBuf>,
    reset: &AtomicBool,
) -> Result<()> {
    while state.is_running() {
        if reset.load(Ordering::Acquire) {
            samples.discard_buffered();
            let mut frame = display
                .lock()
                .map_err(|_| anyhow!("Display lock poisoned"))?;
            let revision = frame.revision.wrapping_add(1);
            *frame = Display::waiting();
            frame.revision = revision;
            drop(frame);
            reset.store(false, Ordering::Release);
            eprintln!("Decoder reset. Listening for a new transmission header.");
        }
        receive(
            samples.by_ref(),
            sample_rate,
            state,
            display,
            output.clone(),
            reset,
        )?;
        if !reset.load(Ordering::Acquire) {
            break;
        }
    }
    Ok(())
}

struct DecoderApp {
    display: Arc<Mutex<Display>>,
    state: Arc<RunState>,
    reset: Arc<AtomicBool>,
    dropped: Arc<AtomicUsize>,
    worker_finished: Arc<AtomicBool>,
    texture: Option<egui::TextureHandle>,
    last_dropped: usize,
    last_xruns: usize,
    uploaded_revision: Option<usize>,
    snapshot: Vec<u32>,
    last_title: String,
    color_image: Arc<egui::ColorImage>,
}

impl DecoderApp {
    fn refresh_texture(&mut self, ctx: &egui::Context) -> Result<()> {
        let frame = self
            .display
            .lock()
            .map_err(|_| anyhow!("Display lock poisoned"))?;
        if self.last_title != frame.title {
            self.last_title.clone_from(&frame.title);
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.last_title.clone()));
        }
        let changed = self.uploaded_revision != Some(frame.revision);
        let size = [frame.width, frame.height];
        if changed {
            self.snapshot.clone_from(&frame.pixels);
            self.uploaded_revision = Some(frame.revision);
        }
        drop(frame);
        if changed {
            let image = Arc::make_mut(&mut self.color_image);
            image.size = size;
            image.source_size = egui::vec2(size[0] as f32, size[1] as f32);
            image
                .pixels
                .resize(self.snapshot.len(), egui::Color32::BLACK);
            for (target, pixel) in image.pixels.iter_mut().zip(&self.snapshot) {
                *target =
                    egui::Color32::from_rgb((pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8);
            }
            if let Some(texture) = &mut self.texture {
                texture.set(Arc::clone(&self.color_image), egui::TextureOptions::NEAREST);
            } else {
                self.texture = Some(ctx.load_texture(
                    "reception",
                    Arc::clone(&self.color_image),
                    egui::TextureOptions::NEAREST,
                ));
            }
        }
        Ok(())
    }
}

impl eframe::App for DecoderApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if !self.state.is_running()
            || self.worker_finished.load(Ordering::Acquire)
            || ctx.input(|input| input.key_pressed(egui::Key::Escape))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Err(error) = self.refresh_texture(ctx) {
            eprintln!("{error:#}");
            self.state.stop();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let resetting = self.reset.load(Ordering::Acquire);
                if ui
                    .add_enabled(!resetting, egui::Button::new("Reset"))
                    .clicked()
                {
                    self.reset.store(true, Ordering::Release);
                }
                ui.label(if resetting {
                    "Resetting reception..."
                } else {
                    "Reset before starting a new transmission to detect its protocol."
                });
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
        let missed = self.dropped.load(Ordering::Relaxed);
        if missed != self.last_dropped {
            eprintln!(
                "Warning: input buffer overflow, {missed} samples lost; image may be damaged."
            );
            self.last_dropped = missed;
        }
        let xruns = self.state.xruns.load(Ordering::Relaxed);
        if xruns != self.last_xruns {
            eprintln!("Warning: JACK xrun count {xruns}; image may be damaged.");
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
        .context("Cannot connect to JACK; start a JACK server or use pw-jack")?;
    let sample_rate = client.sample_rate();
    let input = client.register_port("input", jack::AudioIn::default())?;
    let port_name = input.name()?;
    let (mut producer, consumer) = rtrb::RingBuffer::new(2 * sample_rate as usize);
    let dropped = Arc::new(AtomicUsize::new(0));
    let callback_dropped = Arc::clone(&dropped);
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, scope: &jack::ProcessScope| {
            let mut missed = 0;
            for &sample in input.as_slice(scope) {
                if producer.push(pcm_sample(sample)).is_err() {
                    missed += 1;
                }
            }
            if missed != 0 {
                callback_dropped.fetch_add(missed, Ordering::Relaxed);
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
        "Input: {port_name} ({sample_rate} Hz). Close the window or press Esc/Ctrl-C to exit."
    );
    if let Some(source) = args.connect {
        active
            .as_client()
            .connect_ports_by_name(&source, &port_name)
            .with_context(|| format!("Cannot connect from {source}"))?;
    }

    let display = Arc::new(Mutex::new(Display::waiting()));
    let worker_display = Arc::clone(&display);
    let worker_state = Arc::clone(&state);
    let reset = Arc::new(AtomicBool::new(false));
    let worker_reset = Arc::clone(&reset);
    let worker_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&worker_finished);
    let samples = LiveSamples::new(consumer, Arc::clone(&state)).with_interrupt(Arc::clone(&reset));
    let worker = std::thread::spawn(move || {
        let result = receive_live(
            samples,
            sample_rate,
            &worker_state,
            &worker_display,
            args.output,
            &worker_reset,
        );
        finished.store(true, Ordering::Release);
        result
    });

    let app = DecoderApp {
        display,
        state: Arc::clone(&state),
        reset,
        dropped,
        worker_finished,
        texture: None,
        last_dropped: 0,
        last_xruns: 0,
        uploaded_revision: None,
        snapshot: Vec::new(),
        last_title: String::new(),
        color_image: Arc::new(egui::ColorImage::new([0, 0], Vec::new())),
    };
    let result = eframe::run_native(
        "SSTV - waiting for a transmission",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 560.0]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(app))),
    )
    .map_err(|error| anyhow!("Cannot run decoder GUI: {error}"));
    state.stop();
    let worker_result = worker
        .join()
        .map_err(|_| anyhow!("Decoder worker panicked"))?;
    state.check()?;
    active
        .deactivate()
        .context("Cannot deactivate JACK client")?;
    result?;
    worker_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use sstv::{Encoder, RgbPixel, Synthesizer, modes::ROBOT_36};

    #[test]
    fn texture_uploads_only_on_picture_changes_including_reset() {
        let display = Arc::new(Mutex::new(Display::waiting()));
        let mut app = DecoderApp {
            display: Arc::clone(&display),
            state: RunState::new(),
            reset: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicUsize::new(0)),
            worker_finished: Arc::new(AtomicBool::new(false)),
            texture: None,
            last_dropped: 0,
            last_xruns: 0,
            uploaded_revision: None,
            snapshot: Vec::new(),
            last_title: String::new(),
            color_image: Arc::new(egui::ColorImage::new([0, 0], Vec::new())),
        };
        let ctx = egui::Context::default();
        let first = ctx.run(Default::default(), |ctx| app.refresh_texture(ctx).unwrap());
        let texture = app.texture.as_ref().unwrap().id();
        assert!(
            first
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture)
        );
        display.lock().unwrap().title = "Title only".into();
        let idle = ctx.run(Default::default(), |ctx| app.refresh_texture(ctx).unwrap());
        assert!(!idle.textures_delta.set.iter().any(|(id, _)| *id == texture));
        {
            let mut frame = display.lock().unwrap();
            frame.pixels[0] = 0xff_ffff;
            frame.revision += 1;
        }
        let changed = ctx.run(Default::default(), |ctx| app.refresh_texture(ctx).unwrap());
        assert!(
            changed
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture)
        );
        assert_eq!(app.color_image.pixels[0], egui::Color32::WHITE);
        {
            let mut frame = display.lock().unwrap();
            let revision = frame.revision + 1;
            *frame = Display::waiting();
            frame.revision = revision;
        }
        let reset = ctx.run(Default::default(), |ctx| app.refresh_texture(ctx).unwrap());
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
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn reset_interrupts_idle_and_partial_reception_then_detects_a_new_protocol() {
        let state = RunState::new();
        let reset = Arc::new(AtomicBool::new(false));
        let display = Arc::new(Mutex::new(Display::waiting()));
        let (mut producer, consumer) = rtrb::RingBuffer::new(4_000_000);
        let samples =
            LiveSamples::new(consumer, Arc::clone(&state)).with_interrupt(Arc::clone(&reset));
        let worker_state = Arc::clone(&state);
        let worker_reset = Arc::clone(&reset);
        let worker_display = Arc::clone(&display);
        let worker = std::thread::spawn(move || {
            receive_live(
                samples,
                48_000,
                &worker_state,
                &worker_display,
                None,
                &worker_reset,
            )
        });

        reset.store(true, Ordering::Release);
        let idle_reset = wait_until(|| !reset.load(Ordering::Acquire));
        for sample in transmission(false, 48_000).into_iter().take(4 * 48_000) {
            producer.push(sample).unwrap();
        }
        let first_detected = wait_until(|| display.lock().unwrap().title.contains("Robot36"));
        // Let the partial image run out of queued samples, leaving next() blocked.
        let input_consumed = wait_until(|| producer.slots() == 4_000_000);
        reset.store(true, Ordering::Release);
        let partial_reset = wait_until(|| !reset.load(Ordering::Acquire));
        let cleared = {
            let frame = display.lock().unwrap();
            frame.title.contains("waiting") && frame.pixels.iter().all(|&pixel| pixel == 0)
        };
        let encoder = Encoder::new(
            sstv::modes::MARTIN_2,
            std::iter::repeat_n(RgbPixel::new(100, 150, 200), 320 * 256),
        )
        .unwrap();
        for sample in Synthesizer::new(encoder, 48_000).chain(std::iter::repeat_n(0, 96_000)) {
            producer.push(sample).unwrap();
        }
        let next_complete = wait_until(|| {
            let frame = display.lock().unwrap();
            frame.title.contains("Martin2") && frame.title.contains("complete")
        });
        state.stop();
        worker.join().unwrap().unwrap();
        assert!(idle_reset, "Reset did not interrupt header search");
        assert!(
            first_detected && input_consumed,
            "Partial Robot36 was not received"
        );
        assert!(
            partial_reset && cleared,
            "Reset did not cancel and clear the partial image"
        );
        assert!(
            next_complete,
            "New protocol was not detected after resetting"
        );
    }

    fn transmission(inverted: bool, sample_rate: u32) -> Vec<i16> {
        let pixels = (0..240).flat_map(move |y| {
            (0..320).map(move |x| {
                let value = ((x + y) * 255 / 558) as u8;
                let value = if inverted { 255 - value } else { value };
                RgbPixel::new(value, value, value)
            })
        });
        let encoder = Encoder::new(ROBOT_36, pixels).unwrap();
        let mut samples: Vec<i16> = Synthesizer::new(encoder, sample_rate)
            .map(|sample| pcm_sample(f32::from(sample) / f32::from(i16::MAX) * 0.8))
            .collect();
        samples.resize(samples.len() + 2 * sample_rate as usize, 0);
        samples
    }

    #[test]
    fn receives_consecutive_images_through_jack_sample_conversion() {
        let sample_rate = 48_000;
        let samples = transmission(false, sample_rate)
            .into_iter()
            .chain(transmission(true, sample_rate));
        let state = RunState::new();
        let display = Mutex::new(Display::waiting());
        receive(
            samples,
            sample_rate,
            &state,
            &display,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        let frame = display.lock().unwrap();
        assert_eq!((frame.width, frame.height), (320, 240));
        assert!(frame.title.contains("complete"));
        assert!(frame.title.contains("Robot36"));
        let error: f64 = frame
            .pixels
            .iter()
            .enumerate()
            .map(|(index, pixel)| {
                let expected = 255 - ((index % 320 + index / 320) * 255 / 558) as i32;
                let channels = [(pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255];
                channels
                    .iter()
                    .map(|&channel| (channel as i32 - expected).unsigned_abs() as f64)
                    .sum::<f64>()
            })
            .sum::<f64>()
            / (320.0 * 240.0 * 3.0);
        assert!(error < 8.0, "Round-trip mean channel error: {error}");
    }

    #[test]
    fn silence_does_not_replace_the_display() {
        let state = RunState::new();
        let display = Mutex::new(Display::waiting());
        receive(
            std::iter::repeat_n(0, 48_000),
            48_000,
            &state,
            &display,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(display.lock().unwrap().title.contains("waiting"));
    }
}
