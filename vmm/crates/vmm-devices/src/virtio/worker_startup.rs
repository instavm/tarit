//! Startup handshake of the device I/O workers.
//!
//! A worker installs its seccomp sandbox before it touches the device, then
//! reports success or the error that stopped it. Waiting for that report is
//! separate from starting the thread so a VMM can start every worker first
//! and collect their reports together: each sandbox takes a fraction of a
//! millisecond to build, and waiting for them one at a time put those on
//! the boot and restore path back to back.

use std::io;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

/// A worker's startup report, not yet received.
pub struct PendingStartup {
    ready: Receiver<io::Result<()>>,
    worker: &'static str,
}

impl PendingStartup {
    pub(crate) fn new(ready: Receiver<io::Result<()>>, worker: &'static str) -> Self {
        Self { ready, worker }
    }

    /// Wait for the report. A worker that failed has exited; it is joined.
    pub(crate) fn wait(self, handle: &mut Option<JoinHandle<()>>) -> io::Result<()> {
        let failure = match self.ready.recv() {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => error,
            Err(error) => {
                io::Error::other(format!("{} exited during startup: {error}", self.worker))
            }
        };
        if let Some(handle) = handle.take() {
            let _ = handle.join();
        }
        Err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_start_is_reported_and_the_worker_joined() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut handle = Some(std::thread::spawn(move || {
            let _ = tx.send(Err(io::Error::other("sandbox refused")));
        }));
        let error = PendingStartup::new(rx, "test worker")
            .wait(&mut handle)
            .unwrap_err();
        assert_eq!(error.to_string(), "sandbox refused");
        assert!(handle.is_none(), "joined");
    }

    #[test]
    fn a_worker_that_exits_without_reporting_is_an_error() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
        let mut handle = Some(std::thread::spawn(move || drop(tx)));
        let error = PendingStartup::new(rx, "test worker")
            .wait(&mut handle)
            .unwrap_err();
        assert!(error
            .to_string()
            .starts_with("test worker exited during startup"));
    }

    #[test]
    fn a_ready_worker_keeps_running() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut handle = Some(std::thread::spawn(move || {
            let _ = tx.send(Ok(()));
        }));
        PendingStartup::new(rx, "test worker")
            .wait(&mut handle)
            .unwrap();
        assert!(handle.is_some(), "not joined");
        handle.unwrap().join().unwrap();
    }
}
