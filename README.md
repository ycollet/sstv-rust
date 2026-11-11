# SSTV over JACK

Two independent Rust applications for still-image experiments:

```text
sstv-encoder:output -> [optional JACK audio effects] -> sstv-decoder:input
```

The encoder transmits a PNG or JPEG using a selectable SSTV protocol (Robot 36
by default). The receiver
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
    libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev
cargo build --release
```

Start a JACK server using your usual audio configuration (for example QjackCtl).
Alternatively, with PipeWire's JACK compatibility installed, prefix each
application with `pw-jack`. Both must use the same server. A graphical desktop
is required by the encoder GUI and receiver. Neither application starts an
audio server.

## Direct transmission

Start the receiver first:

```sh
./target/release/sstv-decoder --output received.png
```

In another terminal:

```sh
./target/release/sstv-encoder picture.png --connect sstv-decoder:input
```

The encoder opens a control window. Connect the output, select a **Protocol**,
then click **Play** to transmit the still image. **Stop** cancels transmission;
the next Play starts again from the image header. The window stays open after
the image completes. Protocol selection is available while stopped, and audio
preparation runs in a worker so the window remains responsive.

The **Repeat** checkbox retransmits with two seconds of silence between images.
It can be changed while playing; unchecking it finishes the current image.
`--repeat` sets its initial value. `--protocol Martin1` (or `PD-120`, `Scottie1`,
etc.) selects the initial protocol. All 18 modes supported by the SSTV crate
are available in the selector.

The receiver stays open; Esc, Ctrl-C,
or closing its window exits. `--output` is optional and saves each complete
image, overwriting the previous one. PNG is recommended for the output.
`--name` changes either application's JACK client name.

To connect manually, omit `--connect` and use a JACK patchbay or:

```sh
jack_connect sstv-encoder:output sstv-decoder:input
```

The applications print their actual port names at startup; JACK may rename
clients when multiple instances are running.

## JACK transport

**Follow JACK transport** is off by default. When enabled:

- Rolling starts a transmission from its header, including when enabling Follow
  while JACK is already rolling or when the initial audio finishes preparing.
- Stopped/starting cancels transmission. The next rolling transition restarts
  from the header; it does not resume a partial image.
- A completed image stays stopped until the next transport start unless Repeat
  is checked. Repeat continues only while transport is rolling.
- Turning Follow off stops local playback. Play becomes available for manual use.

Play is disabled while following transport. Stop can cancel the local
transmission even while JACK stays rolling; the next transport start triggers
it again. Neither button changes the global JACK transport. Timeline seeking,
tempo, MIDI transport, and sample-position chasing are not implemented.
Wire the full effects chain before starting transport. The follow behavior
does not wait for a port connection.

`--follow-transport` enables the checkbox on startup.

## Headless encoder

Use `--headless` for the previous command-line workflow:

```sh
./target/release/sstv-encoder picture.png --headless \
    --protocol Robot36 --connect sstv-decoder:input
```

Without transport following, it waits for an output connection, sends one image,
and exits. `--repeat` repeats until Ctrl-C. With `--follow-transport`, it remains
running and responds to JACK start/stop events instead of starting on connection.

## Effects

Prepare the receiver and effects chain first. Connect the encoder output to the
chain before pressing Play or starting JACK transport. In headless mode without
transport following, connect the encoder **last**: its first connection starts
transmission, including the header needed for mode detection.
Use Repeat to try effects over several receptions.
One mono audio channel is sufficient; avoid unnecessary stereo processing.
The encoder sends at 80% of full scale.

Try noise, filtering, delay, frequency shifts, or dropouts. Heavy processing can
destroy the header or line synchronization; no received image is then a valid
outcome. Bypass effects and resend to establish a clean baseline. The display
retains the last image while searching for the next header.

## Implementation and limitations

Encoding is precomputed outside JACK callbacks, in a worker for the GUI. Output
callbacks only copy samples; input callbacks convert samples into a bounded,
lock-free two-second ring buffer. A worker decodes rows using
[`sstv`](https://github.com/unexcellent/sstv); the main thread displays them
with minifb. The encoder GUI uses egui/eframe with an OpenGL renderer. Protocol
changes transfer prepared audio through bounded lock-free queues; old audio is
freed outside the JACK callback. Audio is never routed to speakers automatically.

Both applications use JACK's negotiated sample rate. Sample-rate changes require
a restart. JACK xruns and receiver buffer overflows are reported on stderr;
lost audio may damage the current image. No samples are silently resampled.
Clean reception is still lossy because of SSTV's resolution, chroma sampling,
and frequency demodulation.

This version does not handle MP4, provide an
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
