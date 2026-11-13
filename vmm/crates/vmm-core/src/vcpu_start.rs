//! Cancellable startup gate: restore every vCPU before any KVM_RUN.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    ready: usize,
    released: bool,
    cancelled: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

pub(crate) struct VcpuStartGate {
    shared: Arc<Shared>,
    expected: usize,
}

pub(crate) struct VcpuStartToken(Arc<Shared>);

impl VcpuStartGate {
    pub(crate) fn new(expected: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
            expected,
        }
    }

    pub(crate) fn participant(&self) -> VcpuStartToken {
        VcpuStartToken(self.shared.clone())
    }

    pub(crate) fn release_when_ready(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.ready < self.expected && !state.cancelled {
            let Some(remaining) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                state.cancelled = true;
                self.shared.changed.notify_all();
                return Err("restored vCPU startup timed out".into());
            };
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if state.cancelled {
            return Err("restored vCPU startup cancelled".into());
        }
        state.released = true;
        self.shared.changed.notify_all();
        Ok(())
    }
}

impl Drop for VcpuStartGate {
    fn drop(&mut self) {
        cancel_unreleased(&self.shared);
    }
}

impl VcpuStartToken {
    pub(crate) fn wait(self, stop: &AtomicBool) -> bool {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ready += 1;
        self.0.changed.notify_all();
        while !state.released && !state.cancelled && !stop.load(Ordering::Acquire) {
            state = self
                .0
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.released && !state.cancelled && !stop.load(Ordering::Acquire)
    }
}

impl Drop for VcpuStartToken {
    fn drop(&mut self) {
        cancel_unreleased(&self.0);
    }
}

fn cancel_unreleased(shared: &Shared) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.released {
        state.cancelled = true;
        shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_worker_runs_before_last_vcpu_is_ready() {
        let gate = VcpuStartGate::new(2);
        let bsp = gate.participant();
        let ap = gate.participant();
        let (ran_tx, ran_rx) = std::sync::mpsc::channel();
        let bsp_thread = std::thread::spawn(move || {
            assert!(bsp.wait(&AtomicBool::new(false)));
            ran_tx.send(()).unwrap();
        });
        assert!(ran_rx.recv_timeout(Duration::from_millis(30)).is_err());
        let ap_thread = std::thread::spawn(move || assert!(ap.wait(&AtomicBool::new(false))));
        gate.release_when_ready(Duration::from_secs(1)).unwrap();
        drop(gate); // Successful release must survive owner cleanup.
        ran_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        bsp_thread.join().unwrap();
        ap_thread.join().unwrap();
    }

    #[test]
    fn failed_ap_setup_cancels_parked_bsp() {
        let gate = VcpuStartGate::new(2);
        let bsp = gate.participant();
        let failed_ap = gate.participant();
        let thread = std::thread::spawn(move || bsp.wait(&AtomicBool::new(false)));
        drop(failed_ap); // Includes spawn, sandbox-install, or setup unwind.
        assert!(gate.release_when_ready(Duration::from_secs(1)).is_err());
        assert!(!thread.join().unwrap());
    }

    #[test]
    fn stop_during_park_unblocks_without_owner_release() {
        let gate = VcpuStartGate::new(2);
        let token = gate.participant();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(token.wait(&worker_stop)).unwrap());
        let mut state = gate.shared.state.lock().unwrap();
        while state.ready == 0 {
            let (next, timeout) = gate
                .shared
                .changed
                .wait_timeout(state, Duration::from_secs(1))
                .unwrap();
            assert!(!timeout.timed_out(), "worker did not park");
            state = next;
        }
        drop(state);
        stop.store(true, Ordering::Release);
        assert!(!rx.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        assert!(gate.release_when_ready(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn owner_unwind_stop_and_timeout_unblock_workers() {
        let gate = VcpuStartGate::new(1);
        let token = gate.participant();
        drop(gate);
        assert!(!token.wait(&AtomicBool::new(false)));
        let gate = VcpuStartGate::new(1);
        assert!(!gate.participant().wait(&AtomicBool::new(true)));
        assert!(gate.release_when_ready(Duration::from_millis(20)).is_err());
        let gate = VcpuStartGate::new(2);
        let token = gate.participant();
        let thread = std::thread::spawn(move || token.wait(&AtomicBool::new(false)));
        assert!(gate.release_when_ready(Duration::from_millis(20)).is_err());
        assert!(!thread.join().unwrap());
    }
}
