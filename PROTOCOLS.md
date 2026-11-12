# Supported SSTV protocols

This document describes the 18 analog color SSTV modes supported by the
encoder and decoder through `sstv` 0.3.1, the version in `Cargo.lock`.
It describes that implementation, rather than every historical variant of
these protocols.

The encoder's **Protocol** selector chooses the transmitted mode. The decoder
automatically identifies it from the VIS header; it has no manual mode selector.
Every mode transmits a still image, not a conventional moving-video stream.

For moving video, the separate `nbtv-encoder` and `nbtv-decoder` applications
implement monochrome NBTV Club 32-line at 12.5 fps, not an SSTV mode.
See [NBTV operation and signal format](README.md#nbtv-moving-video-and-still-images).

## Common signal format

SSTV represents image component values as the instantaneous frequency of a
mono audio signal. During image scans:

```text
component value 0   -> 1500 Hz
component value 255 -> 2300 Hz
f(value) = 1500 + floor(800 * value / 255) Hz
```

For RGB modes, the component is red, green, or blue intensity. For
luminance/color-difference modes, it is brightness or a biased color-difference
value; 1500 Hz therefore does not mean "black" for a chroma scan.

The signal also contains control tones: 1200 Hz synchronization, leader tones,
porches, separators, and the VIS mode identifier. Preserve approximately
1100-2300 Hz when establishing a clean reception baseline. This frequency range
is not a specification for a brick-wall filter: transitions and practical
filters also matter.

In this application, the synthesized signal is sent at 80% of full scale using
JACK's sample rate. The audio sample rate is independent of the selected mode:
48 kHz audio, for example, does not make an SSTV image arrive faster.

### Header and automatic mode detection

The encoder sends the following before each image:

| Stage | Signal | Duration |
| --- | --- | --- |
| VOX preamble | 1900, 1500, 1900, 1500, 2300, 1500, 2300, 1500 Hz | 8 x 100 ms |
| First leader | 1900 Hz | 300 ms |
| Break | 1200 Hz | 10 ms |
| Second leader | 1900 Hz | 300 ms |
| VIS start bit | 1200 Hz | 30 ms |
| VIS mode bits | Seven bits, least significant first | 7 x 30 ms |
| VIS parity | Even parity over the seven mode bits | 30 ms |
| VIS stop bit | 1200 Hz | 30 ms |

VIS data and parity use **1100 Hz for 1** and **1300 Hz for 0**. The VIS numbers
below are decimal seven-bit mode codes, excluding the parity bit.

The preamble and header last **1.710 seconds**. Scottie modes add an initial
9 ms synchronization pulse, making their total **1.719 seconds**.
The VOX preamble is included by this encoder; it is not an essential part of
every SSTV implementation's header.

VIS parity checks the mode identifier, not the image. These analog image scans
have no packet retransmission or image-data error correction.

### Timing conventions

A **scan** transmits one component across the image width. A **sequence** carries
one image row, except Robot 36 and PD, whose sequences carry two rows.
A **porch** is a short fixed-frequency interval next to a sync pulse or scan.

The table gives nominal image-body times, including line controls but excluding
the initial preamble/header and this application's trailing two-second silence.
Values are calculated from the crate's timing tables and rounded to three
decimals. Pixel-duration and audio-sample quantization can introduce small
differences in the generated signal.

```text
one playback / repeat cycle = header + image body + 2.000 s silence
```

For Robot 36 this is approximately **39.710 s**; for PD-120, **129.813 s**.
Mode names are conventional labels, not exact transmission durations.

## Mode reference

The CLI accepts the identifiers below case-insensitively and ignores punctuation
and spaces: `--protocol PD-120` is equivalent to `--protocol Pd120`.

| Mode | GUI / CLI identifier | Resolution | VIS | Component organization | Image body (s) |
| --- | --- | --- | --- | --- | --- |
| Robot 36 | `Robot36` | 320 x 240 | 8 | Y + shared chroma per row pair | 36.000 |
| Robot 72 | `Robot72` | 320 x 240 | 12 | Y + both chroma components per row | 72.000 |
| Martin 1 | `Martin1` | 320 x 256 | 44 | G, B, R per row | 114.290 |
| Martin 2 | `Martin2` | 320 x 256 | 40 | G, B, R per row | 58.060 |
| Scottie 1 | `Scottie1` | 320 x 256 | 60 | G, B, R; sync before R | 109.624 |
| Scottie 2 | `Scottie2` | 320 x 256 | 56 | G, B, R; sync before R | 71.089 |
| Scottie DX | `ScottieDx` | 320 x 256 | 76 | G, B, R; sync before R | 268.877 |
| Wrasse SC2-180 | `WrasseSc2180` | 320 x 256 | 55 | R, G, B per row | 182.022 |
| Pasokon P3 | `PasokonP3` | 640 x 496 | 113 | R, G, B per row | 203.050 |
| Pasokon P5 | `PasokonP5` | 640 x 496 | 114 | R, G, B per row | 304.576 |
| Pasokon P7 | `PasokonP7` | 640 x 496 | 115 | R, G, B per row | 406.099 |
| PD-50 | `Pd50` | 320 x 256 | 93 | Y0, Cr, Cb, Y1 per row pair | 49.684 |
| PD-90 | `Pd90` | 320 x 256 | 99 | Y0, Cr, Cb, Y1 per row pair | 89.989 |
| PD-120 | `Pd120` | 640 x 496 | 95 | Y0, Cr, Cb, Y1 per row pair | 126.103 |
| PD-160 | `Pd160` | 512 x 400 | 98 | Y0, Cr, Cb, Y1 per row pair | 160.883 |
| PD-180 | `Pd180` | 640 x 496 | 96 | Y0, Cr, Cb, Y1 per row pair | 187.052 |
| PD-240 | `Pd240` | 640 x 496 | 97 | Y0, Cr, Cb, Y1 per row pair | 248.000 |
| PD-290 | `Pd290` | 800 x 616 | 94 | Y0, Cr, Cb, Y1 per row pair | 288.682 |

`Y` is luminance. `Cr` and `Cb` below mean the crate's scaled and biased
red-minus-luminance and blue-minus-luminance components, called `RY` and `BY` in
its source. `Y0` and `Y1` are the two rows' separate luminance scans. These
terms describe analog component scans, not a compressed digital video format.

## Robot 36 and Robot 72

Robot modes separate brightness from color. The crate approximates luminance
as `Y = 0.30 R + 0.59 G + 0.11 B`, using integer arithmetic, and scales and
biases the two color differences into 0-255. Conversion includes rounding and
clamping, so even a clean round trip is not pixel-exact.

### Robot 36

Each row lasts **150 ms**:

| Step | First row | Second row | Duration |
| --- | --- | --- | --- |
| Sync | 1200 Hz | 1200 Hz | 9 ms |
| Sync porch | 1500 Hz | 1500 Hz | 3 ms |
| Luminance | Y0 | Y1 | 88 ms |
| Chroma separator | 1500 Hz | 2300 Hz | 4.5 ms |
| Chroma porch | 1900 Hz | 1900 Hz | 1.5 ms |
| Chroma scan | Cr | Cb | 44 ms |

A complete sequence lasts **300 ms** and contains two sync pulses.
The color differences are averaged vertically over the two source rows, then
shared when reconstructing both rows. Fine vertical color detail is therefore
lost even without effects.

Chroma scans have half the luminance scan duration. In this crate they still
emit one value per horizontal image position; do not interpret this as a
promise of a particular digital 4:2:0 sampling layout.

Robot 36 is the default and the shortest image transmission supported here,
making it a useful starting point for effects experiments.

### Robot 72

Each row carries both color differences independently and lasts **300 ms**:

```text
sync 9 ms @ 1200 Hz
porch 3 ms @ 1500 Hz
Y scan 138 ms
separator 4.5 ms @ 1500 Hz
porch 1.5 ms @ 1900 Hz
Cr scan 69 ms
separator 4.5 ms @ 2300 Hz
porch 1.5 ms @ 1500 Hz
Cb scan 69 ms
```

Robot 72 avoids Robot 36's vertical sharing of chroma, at twice its image-body
duration. Chroma still occupies shorter scans than luminance.

## Martin 1 and Martin 2

Martin sends full red, green, and blue component scans independently for each
row, in **green, blue, red** order:

```text
sync -> porch -> G -> separator -> B -> separator -> R -> separator
```

Sync is **4.862 ms at 1200 Hz**. The porch and each of the three separators are
**0.572 ms at 1500 Hz**.

| Mode | Each component scan | Complete row |
| --- | --- | --- |
| Martin 1 | 146.432 ms | 446.446 ms |
| Martin 2 | 73.216 ms | 226.798 ms |

Both use 320 x 256 pixels and have no cross-row chroma averaging. Martin 2 trades
shorter scan time for faster transmission. Effects spanning component boundaries
can introduce color errors because the three components are sent at different
times.

## Scottie 1, Scottie 2, and Scottie DX

Scottie also sends **green, blue, red**, but places each row's synchronization
pulse **between blue and red**, rather than at the start of the row:

```text
separator -> G -> separator -> B -> sync -> porch -> R
```

Sync is **9 ms at 1200 Hz**. Both separators and the porch are **1.5 ms at
1500 Hz**. An extra 9 ms sync pulse is sent once, after VIS and before the
first row's sequence.

| Mode | Each component scan | Complete row |
| --- | --- | --- |
| Scottie 1 | 138.240 ms | 428.220 ms |
| Scottie 2 | 88.064 ms | 277.692 ms |
| Scottie DX | 345.600 ms | 1050.300 ms |

All three use 320 x 256 pixels. DX substantially increases the time available
per component, not the pixel dimensions. Its longer scans may be useful for
experiments with a fixed delay, but do not guarantee successful reception under
strong effects.

## Wrasse SC2-180

Wrasse transmits **red, green, blue** without separators between the scans:

```text
sync 5.5225 ms @ 1200 Hz -> porch 0.5 ms @ 1500 Hz
-> R 235 ms -> G 235 ms -> B 235 ms
```

The row period is **711.0225 ms**, at 320 x 256 pixels. There is no cross-row
chroma sharing. Despite the "180" label, the implemented image body lasts
approximately 182.022 seconds.

## Pasokon P3, P5, and P7

Pasokon sends **red, green, blue** at 640 x 496 pixels:

```text
sync -> porch -> R -> porch -> G -> porch -> B -> porch
```

Sync uses 1200 Hz; all four porches use 1500 Hz. Timing differs by sub-mode.
The following are the microsecond-rounded timings used in this implementation:

| Mode | Sync | Each porch | Each component scan | Complete row |
| --- | --- | --- | --- | --- |
| P3 | 5.208 ms | 1.042 ms | 133.333 ms | 409.375 ms |
| P5 | 7.813 ms | 1.563 ms | 200.000 ms | 614.065 ms |
| P7 | 10.417 ms | 2.083 ms | 266.666 ms | 818.747 ms |

The family provides high-resolution RGB scans without shared chroma. P3 is
the fastest; P7 gives the longest scan time at the same resolution. These modes
take several minutes per image, which slows interactive comparisons.

## PD-50, PD-90, PD-120, PD-160, PD-180, PD-240, and PD-290

Each PD sequence transmits **two rows**, with separate luminance but shared
color differences:

```text
sync 20 ms @ 1200 Hz -> porch 2.080 ms @ 1500 Hz
-> Y0 -> Cr -> Cb -> Y1
```

All four component scans in a sequence have the same duration. Cr and Cb
are averaged vertically over the row pair. There are no additional separators
between the four scans.

| Mode | Each component scan | Complete row-pair sequence | Row pairs per image |
| --- | --- | --- | --- |
| PD-50 | 91.520 ms | 388.160 ms | 128 |
| PD-90 | 170.240 ms | 703.040 ms | 128 |
| PD-120 | 121.600 ms | 508.480 ms | 248 |
| PD-160 | 195.584 ms | 804.416 ms | 200 |
| PD-180 | 183.040 ms | 754.240 ms | 248 |
| PD-240 | 244.480 ms | 1000.000 ms | 248 |
| PD-290 | 228.800 ms | 937.280 ms | 308 |

PD is relatively time-efficient for its resolution because it sends four
component scans per two rows, rather than six independent RGB scans. The
tradeoff is shared vertical color detail. The decoder must collect the
sequence's components before reconstructing its pair of RGB rows.

The mode number is not an exact time: PD-120 takes about 126.103 seconds for
image data, and PD-180 about 187.052 seconds. PD-290's larger dimensions explain
why its total time is longer than PD-240 despite its shorter component scans.

## Choosing a mode and applying effects

Use **Robot 36** for short turnaround, **Martin 2** for relatively quick
independent RGB scans, and **PD-120** for higher-resolution experiments without
the several-minute duration of Pasokon RGB modes. These are workflow choices,
not claims that one mode is always more resistant to interference.

Effects operate on the audio waveform before demodulation, not directly on
pixels. In particular:

| Effect | Technical consequence |
| --- | --- |
| Gain | Primarily changes amplitude, not the encoded component value; excessive gain clips and insufficient level can impair reception. |
| Frequency shift | Moves component values and control tones; large shifts can prevent synchronization. |
| Dry/wet delay | Mixes tones from different positions, creating interference and ambiguous instantaneous frequency rather than a simple image overlay. |
| Pure delay, no dry signal | Mainly adds latency if the complete signal is preserved, including its header. |
| Filtering | Can suppress valid image or control frequencies and smear transitions. |
| Dropouts or time stretching | Remove data or alter timing; may damage pixels, row alignment, or mode detection. |

For a first delay experiment, try approximately **5-10% wet, no feedback**,
and keep the first **two seconds of every transmission** dry. This protects the
1.710/1.719-second preamble/header with a small margin. The application does
**not** automatically bypass external effects: arrange that in the effect host.

Protecting the header only helps identify the mode. Sync pulses continue
throughout the image, so strong processing can still destroy reception.
A delay comparable with a row or component duration can mix different colors
or rows. Decode behavior is nonlinear; a delayed audio copy does not necessarily
produce a recognizable shifted copy of the picture.

Source images are resized to the chosen mode's dimensions. A clean round trip
can still lose detail through resizing, color conversion, chroma sharing, and
frequency estimation. Progressive display is not instantaneous: complete
components or row pairs and some decoder lookahead are needed before rows can
be shown.

## References and implementation scope

- [sstv 0.3.1 mode tables](https://github.com/unexcellent/sstv/tree/v0.3.1/src/modes):
  dimensions, VIS identifiers, component layouts, and nominal timing values.
- [Header construction](https://github.com/unexcellent/sstv/blob/v0.3.1/src/modes/mode.rs)
  and [VIS encoding](https://github.com/unexcellent/sstv/blob/v0.3.1/src/modes/vis_code.rs).
- [Color conversion](https://github.com/unexcellent/sstv/blob/v0.3.1/src/image.rs)
  and [row-pair chroma averaging](https://github.com/unexcellent/sstv/blob/v0.3.1/src/encode/rgb_lines.rs).
- The upstream tables cite JL Barber (N7CXI), *Proposal for SSTV Mode
  Specifications*, Dayton SSTV forum, 20 May 2000.
- [Application operation and JACK routing](README.md), including transport,
  repeat, and headless behavior.

Other SSTV families, grayscale modes, digital SSTV, and arbitrary custom timings
are outside this application's current supported set.
