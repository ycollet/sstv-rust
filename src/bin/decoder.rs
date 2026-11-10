use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use minifb::{Key, Scale, Window, WindowOptions};
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
}

impl Display {
    fn waiting() -> Self {
        Self {
            width: 320,
            height: 240,
            pixels: vec![0; 320 * 240],
            title: "SSTV - waiting for a transmission (Esc to exit)".into(),
        }
    }
}

fn receive(
    samples: impl Iterator<Item = i16>,
    sample_rate: u32,
    state: &RunState,
    display: &Mutex<Display>,
    output: Option<PathBuf>,
) -> Result<()> {
    let mut samples = samples;
    while state.is_running() {
        let Some(rows) = Decoder::new(samples.by_ref(), sample_rate).rows() else {
            break;
        };
        let mode = rows.mode();
        let (width, height) = mode.resolution();
        let (width, height) = (width as usize, height as usize);
        eprintln!("Receiving {width}x{height} SSTV image.");
        {
            let mut frame = display
                .lock()
                .map_err(|_| anyhow!("Display lock poisoned"))?;
            frame.width = width;
            frame.height = height;
            frame.pixels = vec![0; width * height];
            frame.title = format!("SSTV - receiving {width}x{height}");
        }
        let mut received = 0;
        for row in rows {
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
            frame.title = format!("SSTV - {width}x{height} - row {received}/{height}");
        }
        if !state.is_running() {
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
            frame.title = format!("SSTV - {width}x{height} complete - listening for next image");
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

fn main() -> Result<()> {
    let args = Args::parse();
    let state = RunState::new();
    state.install_interrupt_handler()?;
    let mut window = Window::new(
        "SSTV - waiting for a transmission",
        320,
        240,
        WindowOptions {
            resize: true,
            scale: Scale::X2,
            ..WindowOptions::default()
        },
    )
    .context("Cannot open display window; a graphical desktop is required")?;
    window.set_target_fps(30);
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
    let samples = LiveSamples::new(consumer, Arc::clone(&state));
    let worker = std::thread::spawn(move || {
        receive(
            samples,
            sample_rate,
            &worker_state,
            &worker_display,
            args.output,
        )
    });

    let result = (|| -> Result<()> {
        let mut last_dropped = 0;
        let mut last_xruns = 0;
        while window.is_open()
            && !window.is_key_down(Key::Escape)
            && state.is_running()
            && !worker.is_finished()
        {
            let (pixels, width, height, title) = {
                let frame = display
                    .lock()
                    .map_err(|_| anyhow!("Display lock poisoned"))?;
                (
                    frame.pixels.clone(),
                    frame.width,
                    frame.height,
                    frame.title.clone(),
                )
            };
            window.set_title(&title);
            window.update_with_buffer(&pixels, width, height)?;
            let missed = dropped.load(Ordering::Relaxed);
            if missed != last_dropped {
                eprintln!(
                    "Warning: input buffer overflow, {missed} samples lost; image may be damaged."
                );
                last_dropped = missed;
            }
            let xruns = state.xruns.load(Ordering::Relaxed);
            if xruns != last_xruns {
                eprintln!("Warning: JACK xrun count {xruns}; image may be damaged.");
                last_xruns = xruns;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    })();
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
        receive(samples, sample_rate, &state, &display, None).unwrap();
        let frame = display.lock().unwrap();
        assert_eq!((frame.width, frame.height), (320, 240));
        assert!(frame.title.contains("complete"));
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
        )
        .unwrap();
        assert!(display.lock().unwrap().title.contains("waiting"));
    }
}
