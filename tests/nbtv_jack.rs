//! Opt-in live smoke test, restricted to an explicitly named test server.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use sstv_jack::{
    Playback,
    nbtv::{self, Clip},
    transmit::{Controls, Transmitter},
};

#[test]
#[ignore = "requires an isolated JACK server; set JACK_DEFAULT_SERVER=sstv-rust-nbtv-test"]
fn jack_transport_routes_decodable_nbtv_and_stops_cleanly() {
    assert_eq!(
        std::env::var("JACK_DEFAULT_SERVER").as_deref(),
        Ok("sstv-rust-nbtv-test"),
        "Do not run the transport test on a shared audio server"
    );
    let (sender, _) = jack::Client::new("tx-nbtv", jack::ClientOptions::NO_START_SERVER).unwrap();
    let (receiver, _) = jack::Client::new("rx-nbtv", jack::ClientOptions::NO_START_SERVER).unwrap();
    let sample_rate = sender.sample_rate();
    let mut output = sender
        .register_port("output", jack::AudioOut::default())
        .unwrap();
    let input = receiver
        .register_port("input", jack::AudioIn::default())
        .unwrap();
    let source = output.name().unwrap();
    let destination = input.name().unwrap();
    let controls = Arc::new(Controls::default());
    controls.follow_transport.store(true, Ordering::Release);
    controls.repeat.store(true, Ordering::Relaxed);
    let (mut transmitter, mut exchange) = Transmitter::new(Arc::clone(&controls));
    let clip = Clip::new(
        (0..nbtv::FRAME_PIXELS * 25)
            .map(|index| (index % 32 * 255 / 31) as u8)
            .collect(),
    )
    .unwrap();
    exchange
        .pending
        .push(Playback::nbtv(Arc::new(clip), sample_rate))
        .unwrap();
    let transport = sender.transport();
    transport.stop().unwrap();
    let frames = Arc::new(AtomicUsize::new(0));
    let signals = Arc::new(AtomicUsize::new(0));
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(sample_rate as usize * 2);
    let overflows = Arc::new(AtomicUsize::new(0));
    let callback_overflows = Arc::clone(&overflows);
    let rx = receiver
        .activate_async(
            (),
            jack::contrib::ClosureProcessHandler::new(
                move |_: &jack::Client, scope: &jack::ProcessScope| {
                    for &sample in input.as_slice(scope) {
                        if producer.push(sample).is_err() {
                            callback_overflows.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    jack::Control::Continue
                },
            ),
        )
        .unwrap();
    let callback_transport = sender.transport();
    let tx = sender
        .activate_async(
            (),
            jack::contrib::ClosureProcessHandler::new(
                move |_: &jack::Client, scope: &jack::ProcessScope| {
                    transmitter.process(
                        output.as_mut_slice(scope),
                        callback_transport.query_state().unwrap() == jack::TransportState::Rolling,
                    );
                    jack::Control::Continue
                },
            ),
        )
        .unwrap();
    tx.as_client()
        .connect_ports_by_name(&source, &destination)
        .unwrap();
    let mut decoder = nbtv::Decoder::new(sample_rate).unwrap();
    let mut drain_for = |duration| {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            while let Ok(sample) = consumer.pop() {
                if sample != 0.0 {
                    signals.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(frame) = decoder.push(sample, nbtv::DEFAULT_THRESHOLD) {
                    for row in frame.as_chunks::<32>().0 {
                        assert!(
                            row[0] < 5 && row[31] > 250,
                            "Raster orientation/levels incorrect"
                        );
                    }
                    frames.fetch_add(1, Ordering::Relaxed);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    drain_for(Duration::from_millis(100));
    let stopped_silence = signals.load(Ordering::Relaxed) == 0;
    transport.start().unwrap();
    drain_for(Duration::from_millis(600));
    let first = frames.load(Ordering::Relaxed);
    transport.stop().unwrap();
    drain_for(Duration::from_millis(100));
    let signals_before = signals.load(Ordering::Relaxed);
    drain_for(Duration::from_millis(100));
    let stopped = signals.load(Ordering::Relaxed) == signals_before;
    transport.start().unwrap();
    drain_for(Duration::from_millis(400));
    transport.stop().unwrap();
    tx.deactivate().unwrap();
    rx.deactivate().unwrap();
    assert!(stopped_silence);
    assert!(
        first >= 3,
        "Only {first} frames decoded during initial transport run"
    );
    assert!(stopped, "Signal did not stop with JACK transport");
    assert!(
        frames.load(Ordering::Relaxed) > first,
        "No frames decoded after restart"
    );
    assert_eq!(overflows.load(Ordering::Relaxed), 0);
}
