#[path = "encoder/gui.rs"]
mod gui;

use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use sstv::{Encoder, Synthesizer, modes};
use sstv_jack::{
    Notifications, Playback, RunState,
    transmit::{Controls, Transmitter},
};

#[derive(Parser)]
#[command(about = "Transmit a PNG or JPEG as SSTV over a mono JACK port")]
struct Args {
    /// Image to transmit (resized to the selected protocol's resolution).
    image: PathBuf,
    /// JACK client name.
    #[arg(long, default_value = "sstv-encoder")]
    name: String,
    /// Destination JACK input port; otherwise connect manually.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect: Option<String>,
    /// Retransmit continuously, with two seconds of silence between images.
    #[arg(long)]
    repeat: bool,
    /// SSTV protocol, e.g. Robot36, Martin1, Scottie1, or PD120.
    #[arg(long, default_value = "Robot36", value_parser = parse_protocol)]
    protocol: usize,
    /// Follow JACK transport start/stop (does not change global transport).
    #[arg(long)]
    follow_transport: bool,
    /// No GUI: transmit when connected, then exit unless repeating or following transport.
    #[arg(long)]
    headless: bool,
}

fn parse_protocol(name: &str) -> std::result::Result<usize, String> {
    let normalize = |name: &str| {
        name.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    modes::ALL
        .iter()
        .position(|mode| normalize(&format!("{mode:?}")) == normalize(name))
        .ok_or_else(|| {
            format!(
                "Unknown protocol {name:?}. Choose from: {}",
                modes::ALL.map(|mode| format!("{mode:?}")).join(", ")
            )
        })
}

fn encode(image: &image::DynamicImage, protocol: usize, sample_rate: u32) -> Result<Playback> {
    let encoder =
        Encoder::from_image(modes::ALL[protocol], image).context("Cannot encode image")?;
    let mut samples: Vec<f32> = Synthesizer::new(encoder, sample_rate)
        .map(|sample| f32::from(sample) / f32::from(i16::MAX) * 0.8)
        .collect();
    // Silence lets a receiver finish its lookahead and separates repeats.
    samples.resize(samples.len() + 2 * sample_rate as usize, 0.0);
    Ok(Playback::new(samples, false))
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
    let controls = Arc::new(Controls::default());
    controls.repeat.store(args.repeat, Ordering::Relaxed);
    controls
        .follow_transport
        .store(args.follow_transport, Ordering::Release);
    let (mut transmitter, mut exchange) = Transmitter::new(Arc::clone(&controls));
    if args.headless {
        exchange
            .pending
            .push(encode(&image, args.protocol, sample_rate)?)
            .map_err(|_| anyhow::anyhow!("Audio preparation queue is full"))?;
    }

    let callback_controls = Arc::clone(&controls);
    let callback_state = Arc::clone(&state);
    let transport = client.transport();
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, scope: &jack::ProcessScope| {
            let buffer = output.as_mut_slice(scope);
            match transport.query_state() {
                Ok(transport_state) => {
                    transmitter.process(buffer, transport_state == jack::TransportState::Rolling);
                    jack::Control::Continue
                }
                Err(_) => {
                    buffer.fill(0.0);
                    callback_controls
                        .transport_failed
                        .store(true, Ordering::Relaxed);
                    callback_state.stop();
                    jack::Control::Quit
                }
            }
        },
    );
    let active = client.activate_async(
        Notifications {
            state: Arc::clone(&state),
            sample_rate,
        },
        process,
    )?;
    eprintln!("Output: {port_name} ({sample_rate} Hz)");
    if let Some(destination) = &args.connect {
        active
            .as_client()
            .connect_ports_by_name(&port_name, destination)
            .with_context(|| format!("Cannot connect to {destination}"))?;
    }

    let result = if args.headless {
        let mut started = false;
        let mut seen_playing = false;
        let mut last_xruns = 0;
        eprintln!("Waiting for a connection or JACK transport. Ctrl-C to exit.");
        (|| -> Result<()> {
            while state.is_running() {
                if !args.follow_transport
                    && !started
                    && controls.ready.load(Ordering::Acquire)
                    && monitor.connected_count()? > 0
                {
                    eprintln!("Transmitting {:?}.", modes::ALL[args.protocol]);
                    controls.play();
                    started = true;
                }
                let playing = controls.playing.load(Ordering::Acquire);
                seen_playing |= playing;
                if !args.follow_transport && seen_playing && !playing {
                    eprintln!("Transmission complete.");
                    break;
                }
                let xruns = state.xruns.load(Ordering::Relaxed);
                if xruns != last_xruns {
                    eprintln!("Warning: JACK xrun count {xruns}; the image may be damaged.");
                    last_xruns = xruns;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        })()
    } else {
        gui::run(gui::EncoderApp::new(
            args,
            image,
            sample_rate,
            Arc::clone(&controls),
            exchange,
            Arc::clone(&state),
            monitor,
        ))
    };
    state.stop();
    let deactivation = active.deactivate().context("Cannot deactivate JACK client");
    state.check()?;
    if controls.transport_failed.load(Ordering::Relaxed) {
        bail!("Cannot query JACK transport");
    }
    result?;
    deactivation?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_names_accept_common_punctuation_and_reject_unknown_modes() {
        for mode in ["robot36", "Robot 36", "ROBOT-36"] {
            assert_eq!(
                parse_protocol(mode).unwrap(),
                parse_protocol("Robot36").unwrap()
            );
        }
        assert!(parse_protocol("PD-120").is_ok());
        assert!(
            parse_protocol("not-a-mode")
                .unwrap_err()
                .contains("Choose from")
        );
    }

    #[test]
    fn selected_protocol_is_announced_in_the_encoded_header() {
        let image = image::DynamicImage::new_rgb8(32, 24);
        for protocol in ["Robot36", "Martin2"] {
            let protocol = parse_protocol(protocol).unwrap();
            let mut playback = encode(&image, protocol, 48_000).unwrap();
            let mut samples = vec![0.0; playback.len()];
            playback.fill(&mut samples);
            let rows = sstv::Decoder::new(samples.into_iter().map(sstv_jack::pcm_sample), 48_000)
                .rows()
                .unwrap();
            assert_eq!(
                format!("{:?}", rows.mode()),
                format!("{:?}", modes::ALL[protocol])
            );
        }
    }
}
