//! Background thumbnail decoding. UI independent: the egui layer turns finished
//! thumbnails into textures.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use photo_copy_core::thumb::{load_thumbnail, Thumbnail};

/// A file and the maximum width/height it should be decoded to.
pub type Key = (PathBuf, u32);

pub struct ThumbLoader {
    requests: Sender<Key>,
    results: Receiver<(Key, Result<Thumbnail, String>)>,
    /// Keys asked for and not forgotten since. Results for other keys are dropped.
    requested: HashSet<Key>,
}

impl ThumbLoader {
    /// Start `workers` decoder threads. `on_result` runs after each decoded file
    /// (the UI uses it to request a repaint).
    pub fn new(workers: usize, on_result: impl Fn() + Send + Sync + 'static) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<Key>();
        let (result_tx, result_rx) = mpsc::channel();
        let request_rx = Arc::new(Mutex::new(request_rx));
        let on_result = Arc::new(on_result);

        for _ in 0..workers.max(1) {
            let request_rx = Arc::clone(&request_rx);
            let result_tx = result_tx.clone();
            let on_result = Arc::clone(&on_result);
            thread::spawn(move || loop {
                // Hold the lock only while waiting for the next request.
                let next = request_rx.lock().unwrap().recv();
                let Ok(key) = next else {
                    return; // loader dropped
                };
                let result = load_thumbnail(&key.0, key.1).map_err(|e| e.to_string());
                if result_tx.send((key, result)).is_err() {
                    return;
                }
                on_result();
            });
        }

        ThumbLoader {
            requests: request_tx,
            results: result_rx,
            requested: HashSet::new(),
        }
    }

    /// Queue a decode unless this key was already requested. Returns whether it was queued.
    pub fn request(&mut self, key: Key) -> bool {
        if !self.requested.insert(key.clone()) {
            return false;
        }
        self.requests.send(key).is_ok()
    }

    /// Finished decodes since the last call, for keys that are still wanted.
    pub fn poll(&mut self) -> Vec<(Key, Result<Thumbnail, String>)> {
        let mut done = Vec::new();
        while let Ok((key, result)) = self.results.try_recv() {
            if self.requested.contains(&key) {
                done.push((key, result));
            }
        }
        done
    }

    /// Forget one key so it can be requested again later.
    pub fn forget(&mut self, key: &Key) {
        self.requested.remove(key);
    }

    /// Forget everything requested so far (e.g. when leaving a folder). Decodes
    /// already running finish, but their results are discarded.
    pub fn forget_all(&mut self) {
        self.requested.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    fn png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> PathBuf {
        let path = dir.join(name);
        image::RgbImage::new(w, h).save(&path).unwrap();
        path
    }

    fn wait_for(loader: &mut ThumbLoader, count: usize) -> Vec<(Key, Result<Thumbnail, String>)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        while all.len() < count && Instant::now() < deadline {
            all.extend(loader.poll());
            thread::sleep(Duration::from_millis(5));
        }
        all
    }

    #[test]
    fn decodes_requested_files_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let a = png(dir.path(), "a.png", 400, 200);
        let b = png(dir.path(), "b.png", 50, 50);
        let mut loader = ThumbLoader::new(2, || {});

        assert!(loader.request((a.clone(), 100)));
        assert!(loader.request((b.clone(), 100)));
        let results = wait_for(&mut loader, 2);

        assert_eq!(results.len(), 2);
        let dims = |p: &PathBuf| {
            let (_, r) = results.iter().find(|((path, _), _)| path == p).unwrap();
            let t = r.as_ref().unwrap();
            (t.width, t.height)
        };
        assert_eq!(dims(&a), (100, 50));
        assert_eq!(dims(&b), (50, 50));
    }

    #[test]
    fn same_key_is_only_decoded_once_but_sizes_are_separate() {
        let dir = tempfile::tempdir().unwrap();
        let a = png(dir.path(), "a.png", 400, 200);
        let mut loader = ThumbLoader::new(1, || {});

        assert!(loader.request((a.clone(), 100)));
        assert!(!loader.request((a.clone(), 100)));
        assert!(loader.request((a.clone(), 200)));

        assert_eq!(wait_for(&mut loader, 2).len(), 2);
        thread::sleep(Duration::from_millis(50));
        assert!(loader.poll().is_empty());
    }

    #[test]
    fn failures_are_reported_as_errors() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.jpg");
        fs::write(&bad, b"nope").unwrap();
        let mut loader = ThumbLoader::new(1, || {});

        loader.request((bad, 64));
        let results = wait_for(&mut loader, 1);

        assert!(results[0].1.is_err());
    }

    #[test]
    fn forgotten_results_are_discarded_and_can_be_requested_again() {
        let dir = tempfile::tempdir().unwrap();
        let a = png(dir.path(), "a.png", 40, 40);
        let mut loader = ThumbLoader::new(1, || {});

        loader.request((a.clone(), 64));
        loader.forget_all();
        thread::sleep(Duration::from_millis(300));
        assert!(loader.poll().is_empty());

        assert!(loader.request((a, 64)));
        assert_eq!(wait_for(&mut loader, 1).len(), 1);
    }

    #[test]
    fn notifies_when_a_result_is_ready() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let a = png(dir.path(), "a.png", 40, 40);
        let notified = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&notified);
        let mut loader = ThumbLoader::new(1, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        loader.request((a, 64));
        wait_for(&mut loader, 1);
        // The worker hands over the result before it notifies, so wait for the
        // notification too instead of assuming it already happened.
        let deadline = Instant::now() + Duration::from_secs(10);
        while notified.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(notified.load(Ordering::SeqCst), 1);
    }
}
