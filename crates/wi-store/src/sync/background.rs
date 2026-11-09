//! The background sync owner.

use std::time::{Duration, Instant};

use wi_history::SyncRepo;

use crate::error::Result;
use crate::vault::Vault;

/// for transport; stopping joins this thread after its bounded attempt.
pub struct SyncHandle {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl SyncHandle {
    pub fn stop(mut self) {
        self.finish();
    }
    fn finish(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for SyncHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Vault {
    /// Start only for an explicitly configured git-mode vault. Runs at
    /// startup, 60 seconds after the latest observed publication, and at
    /// least every five minutes, subject to persisted transport backoff.
    pub fn start_sync(&self) -> Result<Option<SyncHandle>> {
        if self.storage_mode() != wi_history::StorageMode::Git || !self.sync_status()?.configured {
            return Ok(None);
        }
        let root = self.root().to_path_buf();
        let options = self.options.clone();
        let placement = self.placement.clone();
        let (stop, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let Ok(vault) = Vault::open_placed(root, options, placement) else {
                return;
            };
            let mut last = Instant::now() - Duration::from_secs(300);
            let mut observed = None;
            let mut dirty = None;
            loop {
                let current = (vault.generation().ok(), SyncRepo::head(&vault.history).ok());
                if observed.as_ref() != Some(&current) {
                    observed = Some(current);
                    dirty = Some(Instant::now());
                }
                let now = Instant::now();
                if now.duration_since(last) >= Duration::from_secs(300)
                    || dirty.is_some_and(|at| now.duration_since(at) >= Duration::from_secs(60))
                {
                    match vault.sync_background() {
                        Ok(report) if !report.deferred => {
                            last = Instant::now();
                            dirty = None;
                        }
                        Ok(_) => {
                            last = Instant::now();
                        }
                        Err(_) => {
                            last = Instant::now();
                            if let Ok(status) = vault.sync_status()
                                && let Some(line) = status.health
                            {
                                eprintln!("{line}");
                            }
                        }
                    }
                }
                if !matches!(
                    receive.recv_timeout(Duration::from_secs(1)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        });
        Ok(Some(SyncHandle {
            stop,
            thread: Some(thread),
        }))
    }
}
