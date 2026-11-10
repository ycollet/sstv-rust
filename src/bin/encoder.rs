use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use sstv::{Encoder, Synthesizer, modes::ROBOT_36};
use sstv_jack::{Notifications, Playback, RunState};

#[derive(Parser)]
#[command(about = "Transmit a PNG or JPEG as Robot 36 SSTV over a mono JACK port")]
struct Args {
    /// Image to transmit (resized to 320x240).
    image: PathBuf,
    /// JACK client name.
    #[arg(long, default_value = "sstv-encoder")]
    name: String,
    /// Destination JACK input port; otherwise connect manually before transmission.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect: Option<String>,
    /// Retransmit continuously, with two seconds of silence between images.
    #[arg(long)]
    repeat: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let image = image::open(&args.image)
        .with_context(|| format!("Cannot load image {}", args.image.display()))?;
    let state = RunState::new();
    state.install_interrupt_handler()?;
    let (client, _) = jack::Client::new(&args.name, jack::ClientOptions::NO_START_SERVER)
        .context("Cannot connect to JACK; start a JACK server or use pw-jack")?;
    let sample_rate = client.sample_rate();
    let mut output = client.register_port("output", jack::AudioOut::default())?;
    let port_name = output.name()?;
    let monitor = output.clone_unowned();

    let encoder = Encoder::from_image(ROBOT_36, &image).context("Cannot encode image")?;
    let mut samples: Vec<f32> = Synthesizer::new(encoder, sample_rate)
        .map(|sample| f32::from(sample) / f32::from(i16::MAX) * 0.8)
        .collect();
    // Silence lets a receiver finish its lookahead and separates repeats.
    samples.resize(samples.len() + 2 * sample_rate as usize, 0.0);
    let duration = samples.len() as f64 / sample_rate as f64;
    let mut playback = Playback::new(samples, args.repeat);
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let callback_started = Arc::clone(&started);
    let callback_finished = Arc::clone(&finished);
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, scope: &jack::ProcessScope| {
            let buffer = output.as_mut_slice(scope);
            if callback_started.load(Ordering::Acquire) {
                if playback.fill(buffer) {
                    callback_finished.store(true, Ordering::Release);
                }
            } else {
                buffer.fill(0.0);
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
    eprintln!("Output: {port_name} ({sample_rate} Hz, Robot 36, 320x240)");
    eprintln!(
        "Waiting for a connection. Wire the full effects chain before connecting this output."
    );
    if let Some(destination) = args.connect {
        active
            .as_client()
            .connect_ports_by_name(&port_name, &destination)
            .with_context(|| format!("Cannot connect to {destination}"))?;
    }

    let mut last_xruns = 0;
    while state.is_running() && !finished.load(Ordering::Acquire) {
        if !started.load(Ordering::Relaxed) && monitor.connected_count()? > 0 {
            eprintln!("Transmitting ({duration:.1} seconds including trailing silence).");
            started.store(true, Ordering::Release);
        }
        let xruns = state.xruns.load(Ordering::Relaxed);
        if xruns != last_xruns {
            eprintln!("Warning: JACK xrun count {xruns}; the image may be damaged.");
            last_xruns = xruns;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    state.check()?;
    active
        .deactivate()
        .context("Cannot deactivate JACK client")?;
    if finished.load(Ordering::Acquire) {
        eprintln!("Transmission complete.");
    } else {
        eprintln!("Transmission cancelled.");
    }
    Ok(())
}
