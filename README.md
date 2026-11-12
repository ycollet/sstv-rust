# SSTV and NBTV over JACK

Independent Rust applications for still-image SSTV and moving-video NBTV
experiments:

```text
sstv-encoder:output -> [optional JACK audio effects] -> sstv-decoder:input
nbtv-encoder:output -> [optional JACK audio effects] -> nbtv-decoder:input
```

The two formats are separate: an SSTV decoder cannot receive NBTV, or vice versa.

The [examples directory](examples/README.md) contains a NASA still image and a
small 10-second NASA video excerpt, with source credits and reuse terms.

The encoder transmits a PNG or JPEG using a selectable SSTV protocol (Robot 36
by default). The receiver
detects the SSTV mode from its header and displays decoded rows progressively.
It keeps listening after each image. No radio hardware is required.

See [PROTOCOLS.md](PROTOCOLS.md) for all supported modes, technical signal
formats, timings, resolutions, and advice on applying effects.

This is actual SSTV timing, not video-rate playback: Robot 36 takes about
38 seconds per image including its header, followed by two seconds of silence.
Images are resized to the mode's resolution, which can change their aspect ratio.

## Linux setup

Install Rust 1.88 or newer and JACK development libraries. On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libjack-jackd2-dev \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev libwayland-dev \
    libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev ffmpeg
cargo build --release
```

Start a JACK server using your usual audio configuration (for example QjackCtl).
Alternatively, with PipeWire's JACK compatibility installed, prefix each
application with `pw-jack`. Both must use the same server. A graphical desktop
is required by the encoder GUI and receiver. Neither application starts an
audio server.

## NBTV: moving video and still images

Start the receiver, then the encoder in a second terminal:

```sh
./target/release/nbtv-decoder
./target/release/nbtv-encoder movie.mp4 --connect nbtv-decoder:input
```

In the encoder window, press **Play**. **Stop**, **Repeat**, and opt-in
**Follow JACK transport** work like the SSTV encoder's controls: starting
transport restarts the clip, stopping cancels it, and local buttons never change
global transport. NBTV uses a **32-line monochrome raster at 12.5 frames per
second**. Both windows offer **Classic (embedded sync)**, the default club
format, and **Separate sync (custom)** for effects experiments. Select the same
mode in both applications; there is no automatic negotiation.

PNG/JPEG input also works:

```sh
./target/release/nbtv-encoder picture.png --repeat --connect nbtv-decoder:input
```

A still becomes a two-second clip. Repeat loops the entire clip without the
SSTV encoder's two-second silence. Video is converted to 12.5 fps; source
soundtracks are not transmitted. Images are fitted into a portrait **32 x 48**
grayscale raster with black padding to preserve aspect ratio. This is intentionally
very low-resolution video, not a way to send full-resolution MP4 over audio.

FFmpeg must be available on `PATH` for MP4 and other video formats. PNG/JPEG
does not require FFmpeg. Media is loaded and resized before playback; the GUI
remains responsive while its worker prepares it. Resized video is written to an
automatically cleaned temporary disk cache (approximately 19.2 kB per second,
or 69 MB per hour). Playback keeps only a bounded prefix and read-ahead window
in RAM, rather than the entire video or a much larger audio recording.
Loading long/high-resolution files may take time. Closing the window or pressing
Ctrl-C cancels loading, including when FFmpeg is stalled waiting to produce data.
Playback then runs at JACK's audio clock, not the media loader's speed.

The encoder also supports `--headless`, `--repeat`, `--follow-transport`,
`--name`, and `--connect`. Without transport following, headless playback starts
when connected and exits at the end of the clip unless Repeat is enabled.

The decoder title shows the format and synchronization status. It searches for
frame synchronization continuously, including after signal loss;
**Reset** clears reception and discards buffered input without reconnecting JACK.
Initial acquisition normally needs roughly two frames (160 ms); this is not a
guaranteed end-to-end latency. The first and last source frames may not be
displayed while acquiring sync or waiting for the next boundary.
`--output latest.png` saves completed frames on a separate worker, overwriting
that path at up to video rate. If saving cannot keep up, only the newest pending
frame is kept, and skipped saves are reported on stderr; reception does not wait
for storage. Shutdown finishes the pending save, and save errors are surfaced.
Esc, Ctrl-C, or closing the window exits.

### Separate-sync mode for effects

This custom mode keeps the same resolution, scan order, levels, and frame rate,
but separates the picture from synchronization:

```text
nbtv-encoder:output -> [JACK effects] -> nbtv-decoder:input
nbtv-encoder:sync   -----------------> nbtv-decoder:sync
```

Keep the sync path clean. Picture effects can then distort brightness without
destroying synchronization, even when the picture falls below the sync threshold.
The picture output replaces negative sync pulses with black; the sync output
carries the classic missing-pulse pattern independently of picture content.
The extra `sync` ports are always present but the encoder's sync output is silent
in Classic mode. This is not a standardized NBTV format and requires a receiver
configured for separate sync; it does not add resolution or color.

For a direct connection without effects:

```sh
./target/release/nbtv-decoder --mode separate-sync
./target/release/nbtv-encoder movie.mp4 --mode separate-sync \
    --connect nbtv-decoder:input --connect-sync nbtv-decoder:sync
```

For effects, connect `output` to the effect input instead, and its output to the
decoder's `input`. The decoder's **Sync delay** control delays the clean sync
by **0–2000 ms** to match latency in the picture path; fractional milliseconds
are supported, rounded to the nearest audio sample. For example,
`--sync-delay-ms 25` compensates for a picture delayed by 25 ms. It cannot
simultaneously align multiple dry/wet echoes or compensate changing delay.
The control is ignored in Classic mode. Changing decoder mode or delay resets
reception and clears buffered signal/delay history. Encoder mode changes are
available while stopped. Manual playback requires both output connections in
Separate sync mode; transport following still starts without waiting for wiring.

### NBTV effects and compatibility

Route the NBTV ports through effects using the same JACK patchbay workflow as
SSTV. Do not mix the SSTV and NBTV signals. NBTV encodes brightness as amplitude:
gain, clipping, filtering, and delay affect the picture directly.

Classic mode follows the [NBTV Association club standard](https://www.nbtv.org.uk/standards.htm):
32 vertical scans, bottom to top and right to left, at 400 lines per second.
The encoder uses 200 microsecond negative sync pulses; the pulse between line
32 and line 1 is omitted and replaced with black to identify each frame.
It does **not** use a custom broad frame-sync pulse.

JACK signal levels are **sync -0.32, black 0.0, white +0.8**; the sync excursion
is 40% of the black-to-white range. These are digital signal units, not a
guaranteed analog voltage at a sound card. Active video is linearly interpolated,
with 50 microsecond black porches before and after each active scan. JACK must
run at 32 kHz or higher; 44.1/48 kHz are suitable.

The receiver's **Sync threshold** slider (also `--threshold`, default `-0.12`)
can compensate for moderate level changes. Sync must fall below the threshold
and, in Classic mode, black/active video must stay above it. In Separate sync
mode the threshold applies only to the clean sync input. Changing the slider resets reception.
There is no automatic gain control, DC restoration, or polarity detection;
external recordings/hardware and AC-coupled effects may need level, offset, and
polarity adjustment. External hardware interoperability has not been verified.

There is **no initial two-second header to protect**, unlike SSTV. Line sync
is present throughout the video, so begin with a mostly dry signal and low
feedback. Strong effects can still destroy sync. A path passing the recommended
2 Hz-10 kHz NBTV band is a useful starting point; ordinary audio high-pass
filters can remove important low-frequency picture information. MP3 and other
lossy audio codecs are not appropriate for transparent NBTV transport.

## SSTV: direct transmission

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

The receiver's title bar shows the detected protocol during reception and after
completion. **Reset** cancels the current reception, discards buffered input,
clears the image and protocol, and listens for a new header without reconnecting
JACK. Click Reset before starting the next transmission, especially when
switching protocols after an interrupted or damaged image. Reset does not stop
the encoder or change JACK transport, and an abandoned image is not saved.

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

SSTV encoding is precomputed into a temporary disk cache outside JACK callbacks,
in a worker for the GUI. A background reader feeds bounded, reusable memory
pages to playback; JACK never reads files, allocates cache pages, or waits for
the reader. NBTV uses the same mechanism for its grayscale-frame cache, with
allocation-free raster lookup in its output callback. Input callbacks
transfer samples into bounded, lock-free two-second ring buffers.
A worker decodes SSTV rows using
[`sstv`](https://github.com/unexcellent/sstv); the main thread displays them
using egui/eframe with an OpenGL renderer, as does the encoder GUI. Protocol
changes transfer prepared audio through bounded lock-free queues; old audio is
freed outside the JACK callback. Audio is never routed to speakers automatically.

The cache retains the start of the clip in RAM, so local Play, transport
restarts, and Repeat can begin immediately while the background reader refills
later pages. At 48 kHz, SSTV uses about 750 KiB for cached waveform pages,
independent of protocol duration; NBTV frame pages use about 96 KiB, independent
of video duration. Source-image decoding, application state, and graphics
resources use additional memory. SSTV disk usage is four bytes per audio sample,
including silence (about 75 MiB for Pasokon P7 at 48 kHz).

Temporary caches use the operating system's temporary directory and are removed
when closed. If that directory is RAM-backed (for example a Linux `/tmp` mounted
as `tmpfs`), set `TMPDIR` to an existing writable directory on disk to avoid
storing the cache in RAM. Sufficient free temporary storage is required.
Cache preparation/read failures are reported explicitly and stop playback.
If read-ahead cannot keep up, missing picture/audio samples become silence
without changing the playback clock, and underrun counts are reported on stderr.

Decoder textures are updated only when picture data changes, including Reset;
color conversion and GPU upload happen outside reception locks. Snapshot and
color buffers are reused when possible.

Both applications use JACK's negotiated sample rate. Sample-rate changes require
a restart. JACK xruns and receiver buffer overflows are reported on stderr;
lost audio may damage the current image. No samples are silently resampled.
Clean reception is still lossy because of SSTV's resolution, chroma sampling,
and frequency demodulation.

The SSTV applications do not handle MP4; use the separate NBTV applications for
moving video. Neither format provides an effects UI. The SSTV receiver searches
for the next transmission after completing a decode, or immediately on Reset.
NBTV uses frame-by-frame synchronization but cannot guarantee recovery under
severe signal corruption.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Unit and round-trip tests run without a JACK server or display; the MP4 ingestion
tests require FFmpeg on `PATH`. Actual JACK
routing, audio timing, and desktop rendering must also be exercised on the
target Linux system.

An opt-in NBTV JACK integration test checks both modes and transport start/stop,
including heavy picture distortion with clean separate sync.
Run it only with an isolated dummy server, not your working audio session:

```sh
jackd --no-realtime --name sstv-rust-nbtv-test -d dummy -r 48000 -p 1024
# In another terminal:
JACK_DEFAULT_SERVER=sstv-rust-nbtv-test cargo test --test nbtv_jack -- --ignored
```
