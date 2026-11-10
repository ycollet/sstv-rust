# SSTV over JACK

Two independent Rust applications for still-image experiments:

```text
sstv-encoder:output -> [optional JACK audio effects] -> sstv-decoder:input
```

The encoder transmits a PNG or JPEG using Robot 36 (320x240). The receiver
detects the SSTV mode from its header and displays decoded rows progressively.
It keeps listening after each image. No radio hardware is required.

This is actual SSTV timing, not video-rate playback: Robot 36 takes about
38 seconds per image including its header, followed by two seconds of silence.
Images are resized to the mode's resolution, which can change their aspect ratio.

## Linux setup

Install Rust 1.88 or newer and JACK development libraries. On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libjack-jackd2-dev \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev libwayland-dev \
    libxkbcommon-dev
cargo build --release
```

Start a JACK server using your usual audio configuration (for example QjackCtl).
Alternatively, with PipeWire's JACK compatibility installed, prefix each
application with `pw-jack`. Both must use the same server. A graphical desktop
is required by the receiver. Neither application starts an audio server.

## Direct transmission

Start the receiver first:

```sh
./target/release/sstv-decoder --output received.png
```

In another terminal:

```sh
./target/release/sstv-encoder picture.png --connect sstv-decoder:input
```

The encoder waits until its output is connected, sends one image, and exits.
`--repeat` retransmits until Ctrl-C. The receiver stays open; Esc, Ctrl-C,
or closing its window exits. `--output` is optional and saves each complete
image, overwriting the previous one. PNG is recommended for the output.
`--name` changes either application's JACK client name.

To connect manually, omit `--connect` and use a JACK patchbay or:

```sh
jack_connect sstv-encoder:output sstv-decoder:input
```

The applications print their actual port names at startup; JACK may rename
clients when multiple instances are running.

## Effects

Prepare the receiver and effects chain first. Connect the encoder output to the
chain **last**: its first connection starts transmission, including the header
needed for mode detection. Use `--repeat` to try effects over several receptions.
One mono audio channel is sufficient; avoid unnecessary stereo processing.
The encoder sends at 80% of full scale.

Try noise, filtering, delay, frequency shifts, or dropouts. Heavy processing can
destroy the header or line synchronization; no received image is then a valid
outcome. Bypass effects and resend to establish a clean baseline. The display
retains the last image while searching for the next header.

## Implementation and limitations

Encoding is precomputed before activation, outside JACK callbacks. Output
callbacks only copy samples; input callbacks convert samples into a bounded,
lock-free two-second ring buffer. A worker decodes rows using
[`sstv`](https://github.com/unexcellent/sstv); the main thread displays them
with minifb. Audio is never routed to speakers automatically.

Both applications use JACK's negotiated sample rate. Sample-rate changes require
a restart. JACK xruns and receiver buffer overflows are reported on stderr;
lost audio may damage the current image. No samples are silently resampled.
Clean reception is still lossy because of SSTV's resolution, chroma sampling,
and frequency demodulation.

This initial version does not handle MP4, transmit other modes, provide an
effects UI, or guarantee recovery within a severely corrupted transmission.
The receiver searches for the next transmission after completing a decode.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Unit and round-trip tests run without a JACK server or display. Actual JACK
routing, audio timing, and desktop rendering must also be exercised on the
target Linux system.
