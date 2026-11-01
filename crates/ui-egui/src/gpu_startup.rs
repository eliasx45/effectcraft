//! One detached native startup job. Dropping the receiver retires its result.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::mpsc::{self, Receiver, TryRecvError};

pub(crate) struct Pending<T> {
    result: Receiver<Result<T, String>>,
}

impl<T: Send + 'static> Pending<T> {
    pub(crate) fn start(work: impl FnOnce() -> Result<T, String> + Send + 'static, wake: impl FnOnce() + Send + 'static) -> Result<Self, String> {
        let (send, result) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("effectcraft-gpu-startup".into())
            .spawn(move || {
                let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|payload| {
                    let reason =
                        payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("backend panicked");
                    Err(format!("GPU startup failed: {reason}"))
                });
                // A retired/closed viewer has dropped the receiver; never reinstall it.
                if send.send(value).is_ok() {
                    wake();
                }
            })
            .map_err(|e| format!("cannot start GPU initialization worker: {e}"))?;
        Ok(Self { result })
    }

    pub(crate) fn poll(&self) -> Option<Result<T, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("GPU startup worker disconnected".into())),
        }
    }
}

/// At most one worker. While busy, callers keep offering their newest snapshot;
/// the next poll starts only that newest key, never a queue of obsolete revisions.
pub(crate) struct Latest<K> {
    pending: Option<Pending<()>>,
    started: Option<K>,
    declined: Option<K>,
}
impl<K> Default for Latest<K> {
    fn default() -> Self {
        Self { pending: None, started: None, declined: None }
    }
}
impl<K: Eq> Latest<K> {
    #[cfg(test)]
    pub(crate) fn completed(key: Option<K>) -> Self {
        Self { pending: None, started: key, declined: None }
    }
    pub(crate) fn needs(&mut self, key: &K) -> bool {
        if let Some(pending) = &self.pending {
            let Some(result) = pending.poll() else { return false };
            if let Err(error) = result {
                log::warn!("GPU project warmup: {error}");
            }
            self.pending = None;
        }
        self.started.as_ref() != Some(key) && self.declined.as_ref() != Some(key)
    }
    pub(crate) fn request(&mut self, key: K, work: impl FnOnce() -> Result<(), String> + Send + 'static, wake: impl FnOnce() + Send + 'static) {
        if !self.needs(&key) {
            return;
        }
        match Pending::start(work, wake) {
            Ok(pending) => {
                self.started = Some(key);
                self.declined = None;
                self.pending = Some(pending);
            }
            Err(error) => {
                // Avoid retrying a failed thread spawn every UI frame; a new revision retries.
                self.declined = Some(key);
                log::warn!("GPU project warmup declined until the next revision: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_startup_snapshot_is_not_repeated() {
        let mut latest = Latest::completed(Some(7));
        assert!(!latest.needs(&7));
        assert!(latest.needs(&8));
        latest.declined = Some(8);
        assert!(!latest.needs(&8));
        assert!(latest.needs(&9));
        assert_eq!(latest.started, Some(7));
    }

    #[test]
    fn revisions_coalesce_to_latest_without_parallel_workers() {
        let mut latest = Latest::default();
        let (release, wait) = mpsc::channel();
        let (finished, finished_rx) = mpsc::channel();
        latest.request(
            1,
            move || {
                wait.recv().unwrap();
                Ok(())
            },
            move || finished.send(()).unwrap(),
        );
        for key in 2..10 {
            latest.request(key, || panic!("obsolete worker spawned"), || {});
        }
        release.send(()).unwrap();
        finished_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let (started, started_rx) = mpsc::channel();
        latest.request(
            10,
            move || {
                started.send(()).unwrap();
                Ok(())
            },
            || {},
        );
        started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(latest.started, Some(10));
    }

    #[test]
    fn pending_startup_does_not_wait_and_delivers_once() {
        let (send, wait) = mpsc::channel();
        let (wake, woke) = mpsc::channel();
        let pending = Pending::start(
            move || {
                wait.recv().unwrap();
                Ok(42)
            },
            move || {
                wake.send(()).unwrap();
            },
        )
        .unwrap();
        assert!(pending.poll().is_none());
        send.send(()).unwrap();
        woke.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(pending.poll(), Some(Ok(42)));
    }
    #[test]
    fn retired_startup_never_wakes_or_installs_late_success() {
        let (send, wait) = mpsc::channel();
        let (wake, woke) = mpsc::channel();
        let pending = Pending::start(
            move || {
                wait.recv().unwrap();
                Ok(42)
            },
            move || {
                let _ = wake.send(());
            },
        )
        .unwrap();
        drop(pending);
        send.send(()).unwrap();
        assert!(woke.recv_timeout(std::time::Duration::from_secs(5)).is_err());
    }
    #[test]
    fn startup_failure_is_an_actionable_result() {
        let (wake, woke) = mpsc::channel();
        let pending = Pending::<()>::start(
            || Err("synthetic adapter failure".into()),
            move || {
                wake.send(()).unwrap();
            },
        )
        .unwrap();
        woke.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(pending.poll(), Some(Err("synthetic adapter failure".into())));
    }
}
