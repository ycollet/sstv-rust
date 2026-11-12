use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};

use super::{Clip, FRAME_PIXELS, HEIGHT, WIDTH};
use crate::RunState;

pub fn load(path: &Path, state: &RunState) -> Result<Arc<Clip>> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
    {
        let image =
            image::open(path).with_context(|| format!("Cannot load image {}", path.display()))?;
        let resized = image
            .resize(
                WIDTH as u32,
                HEIGHT as u32,
                image::imageops::FilterType::Lanczos3,
            )
            .to_luma8();
        let mut frame = image::GrayImage::new(WIDTH as u32, HEIGHT as u32);
        image::imageops::overlay(
            &mut frame,
            &resized,
            i64::from((WIDTH as u32 - resized.width()) / 2),
            i64::from((HEIGHT as u32 - resized.height()) / 2),
        );
        // A still is a two-second clip, repeated only when requested.
        return Ok(Arc::new(Clip::new(frame.into_raw().repeat(25))?));
    }
    let mut child = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-an", "-vf",
            "fps=25/2,scale=32:48:force_original_aspect_ratio=decrease,pad=32:48:(ow-iw)/2:(oh-ih)/2,format=gray",
            "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Cannot start ffmpeg; install FFmpeg for MP4/video input")?;
    let mut stderr = child.stderr.take().context("FFmpeg stderr pipe missing")?;
    let errors = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut message = Vec::new();
        stderr.by_ref().take(65_536).read_to_end(&mut message)?;
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok(message)
    });
    let mut output = child.stdout.take().context("FFmpeg output pipe missing")?;
    let mut pixels = Vec::new();
    let read_result = (|| -> Result<()> {
        let mut buffer = [0; FRAME_PIXELS * 16];
        loop {
            ensure!(state.is_running(), "Media loading cancelled");
            let count = output
                .read(&mut buffer)
                .context("Cannot read FFmpeg frames")?;
            if count == 0 {
                break;
            }
            pixels.extend_from_slice(&buffer[..count]);
        }
        Ok(())
    })();
    drop(output);
    if read_result.is_err() && child.try_wait()?.is_none() {
        child.kill().context("Cannot stop FFmpeg")?;
    }
    let status = child.wait().context("Cannot wait for FFmpeg")?;
    let errors = errors
        .join()
        .map_err(|_| anyhow::anyhow!("FFmpeg stderr reader panicked"))??;
    read_result?;
    if !status.success() {
        bail!(
            "FFmpeg failed ({status}): {}",
            String::from_utf8_lossy(&errors)
        );
    }
    Ok(Arc::new(
        Clip::new(pixels).context("Video contains no complete NBTV frames")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(extension: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "sstv-rust-nbtv-media-{}-{}.{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed),
                extension
            )))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_file(&self.0)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("Cannot remove test fixture {}: {error}", self.0.display());
            }
        }
    }

    #[test]
    fn image_input_is_letterboxed_and_held_for_two_seconds() {
        let fixture = Fixture::new("png");
        image::GrayImage::from_pixel(100, 100, image::Luma([200]))
            .save(&fixture.0)
            .unwrap();
        let clip = load(&fixture.0, &RunState::new()).unwrap();
        assert_eq!(clip.frames(), 25);
        assert_eq!(clip.samples(48_000), 96_000);
        assert_eq!(clip.pixels[0], 0);
        assert!(clip.pixels[24 * WIDTH + 16] > 180);
    }

    #[test]
    fn mp4_input_is_resampled_to_twelve_and_a_half_frames_per_second() {
        let fixture = Fixture::new("mp4");
        let result = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x96:rate=25",
                "-t",
                "0.4",
                "-c:v",
                "mpeg4",
            ])
            .arg(&fixture.0)
            .output()
            .expect("FFmpeg is required for the MP4 ingestion test");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let clip = load(&fixture.0, &RunState::new()).unwrap();
        assert_eq!(clip.frames(), 5);
        assert_eq!(clip.samples(44_100), 17_640);
        assert_ne!(
            &clip.pixels[..FRAME_PIXELS],
            &clip.pixels[4 * FRAME_PIXELS..]
        );
    }

    #[test]
    fn invalid_video_surfaces_ffmpeg_diagnostics() {
        let fixture = Fixture::new("mp4");
        std::fs::write(&fixture.0, b"not an MP4").unwrap();
        let error = load(&fixture.0, &RunState::new()).unwrap_err().to_string();
        assert!(error.contains("FFmpeg failed"), "{error}");
        assert!(
            error.contains(fixture.0.file_name().unwrap().to_str().unwrap()),
            "{error}"
        );
    }
}
