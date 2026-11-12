# NASA example media

These files come from NASA's Scientific Visualization Studio,
[Spinning Earth with clouds, atmosphere, and night lights (SVS 5570)](https://svs.gsfc.nasa.gov/5570/),
released July 21, 2025.

**Credit: NASA's Scientific Visualization Studio.**
Data visualizer: Kel Elkins (USRA). Scientist: William Putman (NASA/GSFC).
Blue Marble data: Reto Stockli (NASA/GSFC).

| File | Description | Size |
| --- | --- | --- |
| `nasa-earth.jpg` | Unmodified NASA preview image, 1920 x 1080 | About 372 KiB |
| `nasa-earth.mp4` | First 10 seconds of the NASA visualization, resized to 640 x 360, 12.5 fps, H.264, no audio | About 127 KiB |

## Reuse terms

NASA content generally is not subject to copyright in the United States
(public-domain government material), rather than being distributed under a
Creative Commons license. The source page credits NASA's Scientific
Visualization Studio and lists no third-party copyright restriction for these
assets. See the [NASA Images and Media Usage Guidelines](https://www.nasa.gov/nasa-brand-center/images-and-media/)
for the governing reuse guidance, including third-party exceptions and
restrictions on NASA identifiers and implied endorsement. Copyright treatment
outside the United States may differ.

These are informational examples, not NASA-endorsed software or promotional
assets. Preserve the source credit when redistributing them. The repository's
software license does not relicense these media files.

## Sources and conversion

- [Original JPEG preview](https://svs.gsfc.nasa.gov/vis/a000000/a005500/a005570/Earth_wAtmos_spin_02.01000.preview.jpg)
- [Original 1080p60 MP4](https://svs.gsfc.nasa.gov/vis/a000000/a005500/a005570/Earth_wAtmos_spin_02_1080p60.mp4)

The bundled MP4 is a size-reduced excerpt, not the original NASA file:

```sh
ffmpeg -nostdin -i Earth_wAtmos_spin_02_1080p60.mp4 \
    -t 10 -an -vf 'fps=25/2,scale=640:360' \
    -c:v libx264 -preset medium -crf 26 -pix_fmt yuv420p \
    -movflags +faststart nasa-earth.mp4
```

## Try the examples

With the corresponding decoder already running:

```sh
./target/release/sstv-encoder examples/nasa-earth.jpg --connect sstv-decoder:input
./target/release/nbtv-encoder examples/nasa-earth.mp4 --repeat --connect nbtv-decoder:input
```

Press **Play** in the encoder window. The NBTV encoder converts the video to
its portrait 32 x 48 grayscale raster with black padding.
