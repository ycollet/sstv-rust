use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::JoinHandle;

use anyhow::{Context, Result, ensure};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::POLL_INTERVAL;

const PAGES: usize = 4;

#[derive(Debug)]
struct Page {
    offset: usize,
    valid: usize,
    epoch: usize,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct Requests {
    offset: AtomicUsize,
    epoch: AtomicUsize,
    stop: AtomicBool,
    failed: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct ReadAhead {
    // Keep the head resident so rewind/repeat never waits for a disk seek.
    prefix: Vec<u8>,
    page_bytes: usize,
    ready: Consumer<Page>,
    returned: Producer<Page>,
    current: Option<Page>,
    requests: Arc<Requests>,
    epoch: usize,
    requested_offset: usize,
    worker: Option<JoinHandle<()>>,
}

fn read_page(
    file: &mut File,
    page: &mut Page,
    offset: usize,
    length: usize,
    epoch: usize,
) -> Result<()> {
    page.offset = offset;
    page.epoch = epoch;
    page.valid = page.bytes.len().min(length - offset);
    file.seek(SeekFrom::Start(offset as u64))
        .context("Cannot seek playback cache")?;
    file.read_exact(&mut page.bytes[..page.valid])
        .context("Cannot read playback cache")?;
    Ok(())
}

impl ReadAhead {
    pub(crate) fn new(mut file: File, length: usize, page_bytes: usize) -> Result<Self> {
        ensure!(
            length > 0 && page_bytes > 0,
            "Playback cache must not be empty"
        );
        let mut prefix = vec![0; length.min(page_bytes.saturating_mul(PAGES))];
        file.rewind().context("Cannot rewind playback cache")?;
        file.read_exact(&mut prefix)
            .context("Cannot read playback cache prefix")?;
        let (mut send, ready) = RingBuffer::new(PAGES);
        let (mut returned, mut receive) = RingBuffer::new(PAGES);
        let requests = Arc::new(Requests::default());
        requests.offset.store(prefix.len(), Ordering::Relaxed);
        let mut next = prefix.len();
        for _ in 0..PAGES {
            let mut page = Page {
                offset: 0,
                valid: 0,
                epoch: 0,
                bytes: vec![0; page_bytes],
            };
            if next < length {
                read_page(&mut file, &mut page, next, length, 0)?;
                next += page.valid;
                send.push(page).expect("Initial cache queue has capacity");
            } else {
                returned
                    .push(page)
                    .expect("Initial recycling queue has capacity");
            }
        }
        let worker_requests = Arc::clone(&requests);
        let worker = if prefix.len() == length {
            None
        } else {
            Some(std::thread::spawn(move || {
                let mut epoch = 0;
                let mut spare = None;
                while !worker_requests.stop.load(Ordering::Acquire) {
                    let requested_epoch = worker_requests.epoch.load(Ordering::Acquire);
                    let offset = worker_requests.offset.load(Ordering::Acquire);
                    if epoch != requested_epoch {
                        epoch = requested_epoch;
                        next = offset;
                    } else {
                        next = next.max(offset);
                    }
                    if spare.is_none() {
                        spare = receive.pop().ok();
                    }
                    if next >= length || spare.is_none() {
                        std::thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                    let mut page = spare.take().expect("A recycled page is available");
                    if let Err(error) = read_page(&mut file, &mut page, next, length, epoch) {
                        eprintln!("{error:#}");
                        worker_requests.failed.store(true, Ordering::Release);
                        return;
                    }
                    next += page.valid;
                    send.push(page)
                        .expect("Every page owns a reserved queue slot");
                }
            }))
        };
        let requested_offset = prefix.len();
        Ok(Self {
            prefix,
            page_bytes,
            ready,
            returned,
            current: None,
            requests,
            epoch: 0,
            requested_offset,
            worker,
        })
    }

    pub(crate) fn restart(&mut self) {
        if self.worker.is_none() {
            return;
        }
        if let Some(page) = self.current.take() {
            self.returned
                .push(page)
                .expect("A current page reserves recycling capacity");
        }
        for _ in 0..PAGES {
            let Ok(page) = self.ready.pop() else {
                break;
            };
            self.returned
                .push(page)
                .expect("A ready page reserves recycling capacity");
        }
        // Pages read concurrently with a rewind must not enter the new timeline.
        self.epoch = self.epoch.wrapping_add(1);
        self.requested_offset = self.prefix.len();
        self.requests
            .offset
            .store(self.prefix.len(), Ordering::Release);
        self.requests.epoch.store(self.epoch, Ordering::Release);
    }

    pub(crate) fn bytes_at(&mut self, offset: usize, count: usize) -> Option<&[u8]> {
        if offset < self.prefix.len() {
            return self.prefix.get(offset..offset + count);
        }
        let requested = offset / self.page_bytes * self.page_bytes;
        if requested != self.requested_offset {
            self.requested_offset = requested;
            self.requests.offset.store(requested, Ordering::Release);
        }
        let matches = self.current.as_ref().is_some_and(|page| {
            page.epoch == self.epoch
                && offset >= page.offset
                && offset + count <= page.offset + page.valid
        });
        if !matches {
            if self
                .current
                .as_ref()
                .is_some_and(|page| page.epoch == self.epoch && page.offset > offset)
            {
                return None;
            }
            if let Some(page) = self.current.take() {
                self.returned
                    .push(page)
                    .expect("A current page reserves recycling capacity");
            }
            for _ in 0..PAGES {
                let Ok(page) = self.ready.pop() else {
                    break;
                };
                if page.epoch == self.epoch && page.offset + page.valid > offset {
                    self.current = Some(page);
                    break;
                }
                self.returned
                    .push(page)
                    .expect("A consumed page reserves recycling capacity");
            }
        }
        let page = self.current.as_ref()?;
        let start = offset.checked_sub(page.offset)?;
        page.bytes
            .get(start..start + count)
            .filter(|_| start + count <= page.valid)
    }

    pub(crate) fn failed(&self) -> bool {
        self.requests.failed.load(Ordering::Acquire)
            || self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.requests.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("Playback cache worker panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

    #[test]
    fn bounded_read_ahead_preserves_bytes_and_rewinds_without_stale_pages() {
        let bytes: Vec<u8> = (0..4096).map(|i| (i / 16) as u8).collect();
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&bytes).unwrap();
        let mut cache = ReadAhead::new(file, bytes.len(), 32).unwrap();
        for _ in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(5);
            for (offset, expected) in bytes.iter().enumerate() {
                loop {
                    if let Some(actual) = cache.bytes_at(offset, 1) {
                        assert_eq!(actual[0], *expected, "Offset {offset}");
                        break;
                    }
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
            cache.restart();
        }
    }

    #[test]
    fn late_cache_read_failures_are_reported() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[1; 4096]).unwrap();
        let truncate = file.try_clone().unwrap();
        let mut cache = ReadAhead::new(file, 4096, 32).unwrap();
        truncate.set_len(0).unwrap();
        assert!(cache.bytes_at(128, 1).is_some());
        assert!(cache.bytes_at(160, 1).is_some());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cache.failed() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}
