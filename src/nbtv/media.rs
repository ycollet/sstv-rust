use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use super::{FRAME_PIXELS, HEIGHT, WIDTH};
use crate::{Playback, RunState};

#[derive(Debug)]
pub struct Media {
    file: File,
    frames: usize,
}

impl Media {
    fn new(file: File) -> Result<Self> {
        let bytes = usize::try_from(file.metadata()?.len()).context("Video cache is too large")?;
        ensure!(
            bytes > 0 && bytes.is_multiple_of(FRAME_PIXELS),
            "Video contains no complete NBTV frames"
        );
        Ok(Self {
            file,
            frames: bytes / FRAME_PIXELS,
        })
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn samples(&self, sample_rate: u32) -> usize {
        super::frame_samples(self.frames, sample_rate)
    }

    pub fn into_playback(self, sample_rate: u32) -> Result<Playback> {
        Playback::cached_nbtv(self.file, self.frames, sample_rate)
    }
}

pub fn load(path: &Path, state: &RunState) -> Result<Media> {
    let mut file = tempfile::tempfile().context("Cannot create temporary NBTV cache")?;
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
        for _ in 0..25 {
            file.write_all(frame.as_raw())
                .context("Cannot write NBTV cache")?;
        }
        return Media::new(file);
    }
    let child = Command::new("ffmpeg")
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
    decode_video(child, state, file)
}

fn decode_video(mut child: Child, state: &RunState, file: File) -> Result<Media> {
    let mut stderr = child.stderr.take().context("FFmpeg stderr pipe missing")?;
    let errors = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut message = Vec::new();
        stderr.by_ref().take(65_536).read_to_end(&mut message)?;
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok(message)
    });
    let mut output = child.stdout.take().context("FFmpeg output pipe missing")?;
    let mut writer = BufWriter::new(file);
    let read_failed = AtomicBool::new(false);
    let (read_result, status) = std::thread::scope(|scope| {
        let failed = &read_failed;
        let supervisor = scope.spawn(move || -> Result<_> {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {}
                    Err(error) => {
                        let cleanup = child.kill().and_then(|_| child.wait());
                        if let Err(cleanup) = cleanup {
                            eprintln!("Cannot stop FFmpeg after wait failure: {cleanup}");
                        }
                        return Err(error).context("Cannot query FFmpeg");
                    }
                }
                if !state.is_running() || failed.load(Ordering::Acquire) {
                    child.kill().context("Cannot stop FFmpeg")?;
                    return child.wait().context("Cannot wait for FFmpeg");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
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
                writer
                    .write_all(&buffer[..count])
                    .context("Cannot write NBTV cache")?;
            }
            writer.flush().context("Cannot flush NBTV cache")?;
            Ok(())
        })();
        read_failed.store(read_result.is_err(), Ordering::Release);
        drop(output);
        (read_result, supervisor.join())
    });
    let errors = errors
        .join()
        .map_err(|_| anyhow::anyhow!("FFmpeg stderr reader panicked"))??;
    read_result?;
    let status = status.map_err(|_| anyhow::anyhow!("FFmpeg supervisor panicked"))??;
    ensure!(state.is_running(), "Media loading cancelled");
    if !status.success() {
        bail!(
            "FFmpeg failed ({status}): {}",
            String::from_utf8_lossy(&errors)
        );
    }
    Media::new(writer.into_inner().context("Cannot finish NBTV cache")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};
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
        let mut clip = load(&fixture.0, &RunState::new()).unwrap();
        assert_eq!(clip.frames(), 25);
        assert_eq!(clip.samples(48_000), 96_000);
        let mut pixels = [0; FRAME_PIXELS];
        clip.file.rewind().unwrap();
        clip.file.read_exact(&mut pixels).unwrap();
        assert_eq!(pixels[0], 0);
        assert!(pixels[24 * WIDTH + 16] > 180);
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
        let mut clip = load(&fixture.0, &RunState::new()).unwrap();
        assert_eq!(clip.frames(), 5);
        assert_eq!(clip.samples(44_100), 17_640);
        let mut first = [0; FRAME_PIXELS];
        let mut last = [0; FRAME_PIXELS];
        clip.file.rewind().unwrap();
        clip.file.read_exact(&mut first).unwrap();
        clip.file
            .seek(SeekFrom::Start((4 * FRAME_PIXELS) as u64))
            .unwrap();
        clip.file.read_exact(&mut last).unwrap();
        assert_ne!(first, last);
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

    #[test]
    #[cfg(unix)]
    fn cancellation_interrupts_a_stalled_media_pipe() {
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let state = RunState::new();
        let worker_state = std::sync::Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            decode_video(child, &worker_state, tempfile::tempfile().unwrap())
        });
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        state.stop();
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
