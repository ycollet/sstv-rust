use std::path::PathBuf;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};

use super::{HEIGHT, WIDTH};

#[derive(Default)]
struct Pending {
    frame: Option<Vec<u8>>,
    closed: bool,
}

pub struct FrameSaver {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    skipped: Arc<AtomicUsize>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl FrameSaver {
    pub fn new(path: PathBuf, skipped: Arc<AtomicUsize>) -> Self {
        Self::start(skipped, move |pixels| {
            image::save_buffer(
                &path,
                &pixels,
                WIDTH as u32,
                HEIGHT as u32,
                image::ColorType::L8,
            )
            .with_context(|| format!("Cannot save frame to {}", path.display()))
        })
    }

    fn start(
        skipped: Arc<AtomicUsize>,
        mut save: impl FnMut(Vec<u8>) -> Result<()> + Send + 'static,
    ) -> Self {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let worker_pending = Arc::clone(&pending);
        let worker = std::thread::spawn(move || {
            let (lock, wake) = &*worker_pending;
            loop {
                let mut pending = lock
                    .lock()
                    .map_err(|_| anyhow!("Frame saving lock poisoned"))?;
                while pending.frame.is_none() && !pending.closed {
                    pending = wake
                        .wait(pending)
                        .map_err(|_| anyhow!("Frame saving lock poisoned"))?;
                }
                let Some(frame) = pending.frame.take() else {
                    return Ok(());
                };
                drop(pending);
                save(frame)?;
            }
        });
        Self {
            pending,
            skipped,
            worker: Some(worker),
        }
    }

    pub fn check(&mut self) -> Result<()> {
        if self
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
        {
            self.finish()?;
            bail!("Frame saving worker stopped");
        }
        Ok(())
    }

    pub fn submit(&mut self, pixels: &[u8]) -> Result<()> {
        self.check()?;
        let (lock, wake) = &*self.pending;
        let mut pending = lock
            .lock()
            .map_err(|_| anyhow!("Frame saving lock poisoned"))?;
        if pending.frame.replace(pixels.to_vec()).is_some() {
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }
        wake.notify_one();
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let (lock, wake) = &*self.pending;
        lock.lock()
            .map_err(|_| anyhow!("Frame saving lock poisoned"))?
            .closed = true;
        wake.notify_one();
        worker
            .join()
            .map_err(|_| anyhow!("Frame saving worker panicked"))?
    }
}

impl Drop for FrameSaver {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("{error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn slow_saving_keeps_only_the_latest_pending_frame() {
        let skipped = Arc::new(AtomicUsize::new(0));
        let (started, wait_started) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let (saved, wait_saved) = mpsc::channel();
        let mut saver = FrameSaver::start(Arc::clone(&skipped), move |pixels| {
            if pixels[0] == 1 {
                started.send(()).unwrap();
                wait_release.recv().unwrap();
            }
            saved.send(pixels[0]).unwrap();
            Ok(())
        });
        saver.submit(&[1]).unwrap();
        wait_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        saver.submit(&[2]).unwrap();
        saver.submit(&[3]).unwrap();
        release.send(()).unwrap();
        saver.finish().unwrap();
        assert_eq!(wait_saved.try_iter().collect::<Vec<_>>(), [1, 3]);
        assert_eq!(skipped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn saving_errors_are_returned_to_the_receiver() {
        let mut saver =
            FrameSaver::start(Arc::new(AtomicUsize::new(0)), |_| bail!("Storage failure"));
        saver.submit(&[1]).unwrap();
        assert!(
            saver
                .finish()
                .unwrap_err()
                .to_string()
                .contains("Storage failure")
        );
    }
}
