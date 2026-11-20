# SSTV and NBTV over JACK

Rust encoder and decoder applications for sending pictures through JACK audio
effects and displaying the result. SSTV transmits PNG/JPEG still images using
18 selectable protocols; NBTV transmits low-resolution monochrome video at
12.5 fps, with embedded or separate synchronization.

## Installation

Install Rust 1.88 or newer, JACK development libraries, and FFmpeg for video.
On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libjack-jackd2-dev \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev libwayland-dev \
    libxkbcommon-dev libegl1-mesa-dev libgl1-mesa-dev ffmpeg
cargo build --release
```

Start JACK (for example with QjackCtl). With PipeWire's JACK compatibility,
prefix each application with `pw-jack`. Both applications must use the same
audio server; the GUIs require a graphical desktop.

## Quick start

Start a decoder, then its encoder in a second terminal:

```sh
# SSTV still image
./target/release/sstv-decoder
./target/release/sstv-encoder examples/nasa-earth.jpg --connect sstv-decoder:input
```

```sh
# NBTV moving video
./target/release/nbtv-decoder
./target/release/nbtv-encoder examples/nasa-earth.mp4 --repeat --connect nbtv-decoder:input
```

Press **Play** in the encoder window. SSTV and NBTV use different signals;
connect each encoder to its matching decoder.

## Further documentation

- [Usage guide](DOCUMENTATION.md): controls, JACK routing and transport, effects,
  headless operation, caching, limitations, and development.
- [Protocol reference](PROTOCOLS.md): SSTV and NBTV signal formats and timings.
- [Example media](examples/README.md): NASA sources, credits, and reuse terms.
- [Contributor guidelines](AGENT.md).
- [MIT license](LICENSE) for the software.
