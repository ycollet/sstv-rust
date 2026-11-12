use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::Parser;
use eframe::egui;
use sstv_jack::{
    Notifications, Playback, RunState,
    nbtv::{self, FPS},
    transmit::{AudioExchange, Controls, Transmitter},
};

#[derive(Parser)]
#[command(about = "Transmit MP4 video or a PNG/JPEG as classic 32-line monochrome NBTV over JACK")]
struct Args {
    /// MP4/video file (requires ffmpeg), or PNG/JPEG still image.
    input: PathBuf,
    #[arg(long, default_value = "nbtv-encoder")]
    name: String,
    /// Destination JACK input port.
    #[arg(long, value_name = "CLIENT:PORT")]
    connect: Option<String>,
    /// Loop the clip; a still image is a two-second clip.
    #[arg(long)]
    repeat: bool,
    /// Follow JACK start/stop without changing global transport.
    #[arg(long)]
    follow_transport: bool,
    /// Without a window: start when connected, or follow JACK transport.
    #[arg(long)]
    headless: bool,
}

struct EncoderApp {
    path: String,
    controls: Arc<Controls>,
    exchange: AudioExchange,
    state: Arc<RunState>,
    monitor: jack::Port<jack::Unowned>,
    loader: Option<JoinHandle<Result<Arc<nbtv::Clip>>>>,
    sample_rate: u32,
    frames: usize,
    error: Option<String>,
    last_xruns: usize,
}

impl eframe::App for EncoderApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        while let Ok(previous) = self.exchange.retired.pop() {
            drop(previous);
        }
        if self.loader.as_ref().is_some_and(|job| job.is_finished()) {
            let result = self
                .loader
                .take()
                .expect("loader exists")
                .join()
                .map_err(|_| anyhow!("Media loading worker panicked"))
                .and_then(|result| result)
                .and_then(|clip| {
                    self.frames = clip.frames();
                    self.exchange
                        .pending
                        .push(Playback::nbtv(clip, self.sample_rate))
                        .map_err(|_| anyhow!("Audio preparation queue is full"))
                });
            if let Err(error) = result {
                eprintln!("{error:#}");
                self.error = Some(format!("{error:#}"));
            }
        }
        if !self.state.is_running() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
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
        let ready = self.controls.ready.load(Ordering::Acquire);
        let playing = self.controls.playing.load(Ordering::Acquire);
        let mut repeat = self.controls.repeat.load(Ordering::Relaxed);
        let mut follow = self.controls.follow_transport.load(Ordering::Acquire);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("NBTV encoder");
            ui.label(&self.path);
            ui.label("NBTV Club 32-line | monochrome | 12.5 fps | embedded missing-pulse sync");
            ui.label(format!("{} Hz | {connections} JACK output connection(s)", self.sample_rate));
            ui.separator();
            ui.checkbox(&mut repeat, "Repeat");
            ui.add_enabled_ui(ready, |ui| {
                ui.checkbox(&mut follow, "Follow JACK transport");
            });
            self.controls.repeat.store(repeat, Ordering::Relaxed);
            self.controls.follow_transport.store(follow, Ordering::Release);
            ui.horizontal(|ui| {
                if ui.add_enabled(ready && !playing && !follow && connections > 0, egui::Button::new("Play")).clicked() {
                    self.controls.play();
                }
                if ui.add_enabled(playing, egui::Button::new("Stop")).clicked() {
                    self.controls.stop();
                }
            });
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::RED, error);
            } else if !ready {
                ui.label("Loading and resizing media...");
            } else {
                ui.label(if playing { "Transmitting" } else if follow { "Waiting for JACK transport start" } else { "Ready - connect the effects chain, then press Play" });
                let position = self.controls.position.load(Ordering::Relaxed);
                let duration = self.frames as f64 / FPS;
                let elapsed = position as f64 / f64::from(self.sample_rate);
                ui.add(egui::ProgressBar::new((elapsed / duration) as f32).text(format!("{elapsed:.2} / {duration:.2} s")));
            }
            ui.separator();
            ui.label(format!("JACK transport: {} | xruns: {}", if self.controls.rolling.load(Ordering::Relaxed) { "rolling" } else { "stopped / starting" }, self.state.xruns.load(Ordering::Relaxed)));
            ui.small("Play/Stop are local. Transport start restarts the clip, not its timeline position.");
            ui.small("Portrait 32x48 grayscale; source audio is not transmitted.");
        });
        let xruns = self.state.xruns.load(Ordering::Relaxed);
        if xruns != self.last_xruns {
            eprintln!("Warning: JACK xrun count {xruns}; NBTV synchronization may be damaged.");
            self.last_xruns = xruns;
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl Drop for EncoderApp {
    fn drop(&mut self) {
        self.controls.stop();
        self.state.stop();
        if let Some(loader) = self.loader.take() {
            match loader.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("{error:#}"),
                Err(_) => eprintln!("Media loading worker panicked"),
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let state = RunState::new();
    state.install_interrupt_handler()?;
    let (client, _) = jack::Client::new(&args.name, jack::ClientOptions::NO_START_SERVER)
        .context("Cannot connect to JACK; start a server or use pw-jack")?;
    let sample_rate = client.sample_rate();
    ensure!(
        sample_rate >= 32_000,
        "NBTV requires a JACK sample rate of at least 32000 Hz"
    );
    let mut output = client.register_port("output", jack::AudioOut::default())?;
    let port_name = output.name()?;
    let monitor = output.clone_unowned();
    let controls = Arc::new(Controls::default());
    controls.repeat.store(args.repeat, Ordering::Relaxed);
    controls
        .follow_transport
        .store(args.follow_transport, Ordering::Release);
    let (mut transmitter, mut exchange) = Transmitter::new(Arc::clone(&controls));
    let callback_controls = Arc::clone(&controls);
    let callback_state = Arc::clone(&state);
    let transport = client.transport();
    let process = jack::contrib::ClosureProcessHandler::new(
        move |_: &jack::Client, scope: &jack::ProcessScope| {
            let buffer = output.as_mut_slice(scope);
            match transport.query_state() {
                Ok(transport) => {
                    transmitter.process(buffer, transport == jack::TransportState::Rolling);
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
    eprintln!("Output: {port_name} ({sample_rate} Hz, NBTV Club 32-line, 12.5 fps)");
    if let Some(destination) = &args.connect {
        active
            .as_client()
            .connect_ports_by_name(&port_name, destination)
            .with_context(|| format!("Cannot connect to {destination}"))?;
    }
    let result = (|| -> Result<()> {
        if args.headless {
            let clip = nbtv::media::load(&args.input, &state)?;
            eprintln!(
                "Loaded {} frames ({:.2} s). Waiting for connection or transport.",
                clip.frames(),
                clip.frames() as f64 / FPS
            );
            exchange
                .pending
                .push(Playback::nbtv(clip, sample_rate))
                .map_err(|_| anyhow!("Audio preparation queue is full"))?;
            let mut started = false;
            let mut seen_playing = false;
            let mut last_xruns = 0;
            while state.is_running() {
                if !args.follow_transport
                    && !started
                    && controls.ready.load(Ordering::Acquire)
                    && monitor.connected_count()? > 0
                {
                    controls.play();
                    started = true;
                }
                let playing = controls.playing.load(Ordering::Acquire);
                seen_playing |= playing;
                if !args.follow_transport && seen_playing && !playing {
                    break;
                }
                let xruns = state.xruns.load(Ordering::Relaxed);
                if xruns != last_xruns {
                    eprintln!(
                        "Warning: JACK xrun count {xruns}; NBTV synchronization may be damaged."
                    );
                    last_xruns = xruns;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        } else {
            let load_state = Arc::clone(&state);
            let path = args.input.clone();
            let loader = std::thread::spawn(move || nbtv::media::load(&path, &load_state));
            let app = EncoderApp {
                path: args.input.display().to_string(),
                controls: Arc::clone(&controls),
                exchange,
                state: Arc::clone(&state),
                monitor,
                loader: Some(loader),
                sample_rate,
                frames: 0,
                error: None,
                last_xruns: 0,
            };
            eframe::run_native(
                "NBTV encoder",
                eframe::NativeOptions {
                    viewport: egui::ViewportBuilder::default().with_inner_size([680.0, 330.0]),
                    ..Default::default()
                },
                Box::new(move |_| Ok(Box::new(app))),
            )
            .map_err(|error| anyhow!("Cannot run NBTV encoder GUI: {error}"))?;
        }
        Ok(())
    })();
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
