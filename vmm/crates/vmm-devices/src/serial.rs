//! 16550 serial console via `vm-superio`.

use crate::persist::Persist;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use vm_superio::serial::{NoEvents, Serial as VmSerial, SerialState as VmSerialState};
#[cfg(target_os = "linux")]
use vmm_sys_util::eventfd::EventFd;

const MAX_OUTPUT: usize = 256 * 1024;

#[cfg(not(target_os = "linux"))]
pub struct EventFd;

#[cfg(not(target_os = "linux"))]
impl EventFd {
    pub fn new(_flags: i32) -> io::Result<Self> {
        Ok(Self)
    }

    pub fn write(&self, _v: u64) -> io::Result<()> {
        Ok(())
    }
}

pub struct EventFdTrigger(Arc<EventFd>);

impl EventFdTrigger {
    pub fn new(evt: EventFd) -> Self {
        Self(Arc::new(evt))
    }

    fn from_shared(evt: Arc<EventFd>) -> Self {
        Self(evt)
    }
}

impl vm_superio::Trigger for EventFdTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.0.write(1)
    }
}

pub struct SerialOut {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for SerialOut {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        {
            let mut buf = self.buf.lock().unwrap();
            let retained = if bytes.len() > MAX_OUTPUT {
                &bytes[bytes.len() - MAX_OUTPUT..]
            } else {
                bytes
            };
            let required = buf.len().saturating_add(retained.len());
            if required > MAX_OUTPUT {
                buf.drain(0..required - MAX_OUTPUT);
            }
            buf.extend_from_slice(retained);
        }
        io::stdout().write_all(bytes)?;
        io::stdout().flush()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// Serial (16550 UART) register state that survives snapshot/restore.
///
/// The legacy writable-register shadow remains for snapshots created before
/// complete UART state was recorded. New snapshots also retain pending
/// interrupts, status registers, and the receive FIFO so capture cannot lose a
/// byte or strand a guest waiting for a transmit/receive interrupt.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SerialState {
    /// Interrupt Enable Register (offset 1, DLAB=0). Bit 0 (RX-data-available)
    /// is the exec-critical one: without it a restored UART never signals the
    /// guest that a command byte arrived.
    pub ier: u8,
    /// FIFO Control Register (offset 2, write side).
    pub fcr: u8,
    /// Line Control Register (offset 3), including the DLAB bit.
    pub lcr: u8,
    /// Modem Control Register (offset 4).
    pub mcr: u8,
    /// Scratch register (offset 7).
    pub scr: u8,
    /// Divisor latch low byte (offset 0, DLAB=1).
    pub dll: u8,
    /// Divisor latch high byte (offset 1, DLAB=1).
    pub dlm: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SerialRuntimeState {
    pub interrupt_identification: u8,
    pub line_status: u8,
    pub modem_status: u8,
    #[serde(default)]
    pub in_buffer: Vec<u8>,
}

/// A 16550 UART backed by an EventFd IRQ trigger and a captured stdout sink.
pub struct Serial {
    inner: Mutex<VmSerial<EventFdTrigger, NoEvents, SerialOut>>,
    out_buf: Arc<Mutex<Vec<u8>>>,
    irq_evt: Arc<EventFd>,
    /// Shadow of the guest-programmed writable registers, updated on every
    /// `write`, so the UART configuration can be snapshotted and replayed
    /// for compatibility with snapshots created before full runtime capture.
    shadow: Mutex<SerialState>,
}

impl Serial {
    pub fn new(irq_evt: EventFd) -> Self {
        // Guest console writes run on the seccomp-confined vCPU thread. Keep a
        // fixed-capacity capture buffer so normal boot output never asks the
        // allocator to open glibc tuning files after the filter is installed.
        let out_buf = Arc::new(Mutex::new(Vec::with_capacity(MAX_OUTPUT)));
        let out = SerialOut {
            buf: out_buf.clone(),
        };
        let irq_evt = Arc::new(irq_evt);
        Self {
            inner: Mutex::new(VmSerial::new(
                EventFdTrigger::from_shared(Arc::clone(&irq_evt)),
                out,
            )),
            out_buf,
            irq_evt,
            shadow: Mutex::new(SerialState::default()),
        }
    }

    pub fn read(&self, offset: u8) -> u8 {
        self.inner.lock().unwrap().read(offset)
    }

    pub fn write(&self, offset: u8, val: u8) {
        // Record writable-register programming so it can be replayed on restore.
        // Offsets 0/1 are divisor-latch (DLAB=1) or data/IER (DLAB=0); the DLAB
        // bit lives in the LCR we already shadow.
        {
            let mut sh = self.shadow.lock().unwrap();
            let dlab = sh.lcr & 0x80 != 0;
            match offset {
                0 if dlab => sh.dll = val,
                1 if dlab => sh.dlm = val,
                1 => sh.ier = val,
                2 => sh.fcr = val,
                3 => sh.lcr = val,
                4 => sh.mcr = val,
                7 => sh.scr = val,
                _ => {}
            }
        }
        let _ = self.inner.lock().unwrap().write(offset, val);
    }

    /// Enqueue `bytes` + `'\n'` into the emulated UART RX FIFO, waiting for
    /// the guest to drain it as needed — the FIFO holds only 64 bytes, so a
    /// longer command silently truncates without this. Returns `false` if the
    /// guest did not consume everything before `deadline` (vCPU stalled, IRQs
    /// not being serviced); the input line may then be partially delivered.
    pub fn send_within(&self, bytes: &[u8], deadline: std::time::Instant) -> bool {
        let mut pending = Vec::with_capacity(bytes.len() + 1);
        pending.extend_from_slice(bytes);
        pending.push(b'\n');
        let mut off = 0;
        loop {
            {
                let mut serial = self.inner.lock().unwrap();
                if let Ok(n) = serial.enqueue_raw_bytes(&pending[off..]) {
                    off += n;
                }
            }
            if off == pending.len() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub fn send(&self, bytes: &[u8]) {
        // Best-effort single-shot enqueue; prefer `send_within` for payloads
        // that may exceed the 64-byte UART FIFO.
        let mut serial = self.inner.lock().unwrap();
        let _ = serial.enqueue_raw_bytes(bytes);
        let _ = serial.enqueue_raw_bytes(b"\n");
    }

    pub fn drain_output(&self) -> Vec<u8> {
        // Allocate the replacement on the unrestricted control thread before
        // taking the old buffer. `mem::take` would leave capacity zero and the
        // next guest byte could trigger a forbidden allocator-side openat in
        // the vCPU thread.
        let replacement = Vec::with_capacity(MAX_OUTPUT);
        let mut buf = self.out_buf.lock().unwrap();
        std::mem::replace(&mut *buf, replacement)
    }

    pub fn runtime_state(&self) -> SerialRuntimeState {
        let runtime = self.inner.lock().unwrap().state();
        SerialRuntimeState {
            interrupt_identification: runtime.interrupt_identification,
            line_status: runtime.line_status,
            modem_status: runtime.modem_status,
            in_buffer: runtime.in_buffer,
        }
    }

    pub fn try_restore_snapshot(
        &mut self,
        state: SerialState,
        runtime: Option<&SerialRuntimeState>,
    ) -> Result<(), crate::persist::PersistError> {
        if let Some(runtime) = runtime {
            let restored = VmSerialState {
                baud_divisor_low: state.dll,
                baud_divisor_high: state.dlm,
                interrupt_enable: state.ier,
                interrupt_identification: runtime.interrupt_identification,
                line_control: state.lcr,
                line_status: runtime.line_status,
                modem_control: state.mcr,
                modem_status: runtime.modem_status,
                scratch: state.scr,
                in_buffer: runtime.in_buffer.clone(),
            };
            let out = SerialOut {
                buf: Arc::clone(&self.out_buf),
            };
            *self.inner.get_mut().map_err(|_| {
                crate::persist::PersistError::Unavailable("UART lock is poisoned".into())
            })? = VmSerial::from_state(
                &restored,
                EventFdTrigger::from_shared(Arc::clone(&self.irq_evt)),
                NoEvents,
                out,
            )
            .map_err(|error| {
                crate::persist::PersistError::Unavailable(format!(
                    "reconstruct UART runtime state: {error}"
                ))
            })?;
            *self.shadow.get_mut().map_err(|_| {
                crate::persist::PersistError::Unavailable("UART shadow lock is poisoned".into())
            })? = state;
            return Ok(());
        }

        self.restore_legacy(state)
    }

    fn restore_legacy(&mut self, state: SerialState) -> Result<(), crate::persist::PersistError> {
        // Replay the guest-programmed registers onto the fresh UART, in the
        // order the guest would: program the baud divisor under DLAB=1, then
        // restore the real LCR (which sets the final DLAB), then IER/FCR/MCR/SCR.
        {
            let inner = self.inner.get_mut().map_err(|_| {
                crate::persist::PersistError::Unavailable("UART lock is poisoned".into())
            })?;
            let _ = inner.write(3, state.lcr | 0x80);
            let _ = inner.write(0, state.dll);
            let _ = inner.write(1, state.dlm);
            let _ = inner.write(3, state.lcr);
            let _ = inner.write(2, state.fcr);
            let _ = inner.write(4, state.mcr);
            let _ = inner.write(7, state.scr);
            if state.lcr & 0x80 == 0 {
                let _ = inner.write(1, state.ier);
            }
        }
        *self.shadow.get_mut().map_err(|_| {
            crate::persist::PersistError::Unavailable("UART shadow lock is poisoned".into())
        })? = state;
        Ok(())
    }
}

impl Persist for Serial {
    type State = SerialState;

    fn save(&self) -> Self::State {
        self.shadow.lock().unwrap().clone()
    }

    fn restore(&mut self, state: Self::State) {
        self.try_restore(state)
            .expect("restore captured UART state on a fresh device");
    }

    fn try_restore(&mut self, state: Self::State) -> Result<(), crate::persist::PersistError> {
        self.restore_legacy(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_eventfd() -> EventFd {
        #[cfg(target_os = "linux")]
        let flags = libc::EFD_NONBLOCK;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;

        EventFd::new(flags).unwrap()
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn thr_write_is_captured() {
        let serial = Serial::new(test_eventfd());

        serial.write(0, b'x');

        assert_eq!(serial.drain_output(), b"x");
    }

    #[test]
    fn draining_preserves_preallocated_vcpu_output_capacity() {
        let serial = Serial::new(test_eventfd());
        serial.write(0, b'x');
        assert_eq!(serial.drain_output(), b"x");
        assert!(serial.out_buf.lock().unwrap().capacity() >= MAX_OUTPUT);
    }

    #[test]
    fn enqueued_input_is_read_from_data_register() {
        let serial = Serial::new(test_eventfd());

        serial.send(b"a");

        assert_eq!(serial.read(0), b'a');
    }

    #[test]
    fn persist_round_trip() {
        let mut serial = Serial::new(test_eventfd());
        let state = serial.save();

        serial.restore(state);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn restore_preserves_receive_fifo_and_reasserts_pending_irq() {
        let source_irq = test_eventfd();
        let serial = Serial::new(source_irq.try_clone().unwrap());
        serial.write(1, 0x01); // RX-data-available interrupt.
        serial.send(b"pending");
        assert!(source_irq.read().is_ok(), "source RX interrupt missing");

        let state = serial.save();
        let runtime = serial.runtime_state();
        assert_eq!(
            runtime.in_buffer, b"pending\n",
            "snapshot omitted pending UART input"
        );

        let restored_irq = test_eventfd();
        let mut restored = Serial::new(restored_irq.try_clone().unwrap());
        restored
            .try_restore_snapshot(state, Some(&runtime))
            .expect("restore complete UART state");
        assert!(
            restored_irq.read().is_ok(),
            "restore did not reassert the pending RX interrupt"
        );
        let mut input = Vec::new();
        while restored.read(5) & 1 != 0 {
            input.push(restored.read(0));
        }
        assert_eq!(input, b"pending\n");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn restore_reasserts_pending_transmitter_interrupt() {
        let source_irq = test_eventfd();
        let serial = Serial::new(source_irq.try_clone().unwrap());
        serial.write(1, 0x02); // Transmitter-holding-register-empty interrupt.
        serial.write(0, b'x');
        assert!(source_irq.read().is_ok(), "source TX interrupt missing");

        let restored_irq = test_eventfd();
        let mut restored = Serial::new(restored_irq.try_clone().unwrap());
        restored
            .try_restore_snapshot(serial.save(), Some(&serial.runtime_state()))
            .expect("restore complete UART state");
        assert!(
            restored_irq.read().is_ok(),
            "restore did not reassert the pending TX interrupt"
        );
        assert_ne!(restored.read(2) & 0x02, 0, "restored TX cause missing");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn restore_rejects_an_oversized_receive_fifo() {
        let mut serial = Serial::new(test_eventfd());
        let runtime = SerialRuntimeState {
            in_buffer: vec![0; 65],
            ..Default::default()
        };
        let error = serial
            .try_restore_snapshot(SerialState::default(), Some(&runtime))
            .expect_err("oversized UART FIFO must fail closed");
        assert!(error.to_string().contains("FIFO"));
    }

    #[test]
    fn send_within_delivers_past_the_64_byte_fifo() {
        // The UART RX FIFO holds 64 bytes; a payload longer than that needs
        // the guest to drain in between. Simulate the guest reading the data
        // register from another thread.
        let serial = std::sync::Arc::new(Serial::new(test_eventfd()));
        let payload = vec![b'a'; 200];
        let reader = {
            let serial = std::sync::Arc::clone(&serial);
            std::thread::spawn(move || {
                let mut got = Vec::new();
                while got.last() != Some(&b'\n') {
                    // LSR bit 0 = data ready.
                    if serial.read(5) & 1 != 0 {
                        got.push(serial.read(0));
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                got
            })
        };
        let ok = serial.send_within(
            &payload,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
        assert!(ok, "send_within should complete while the guest drains");
        let mut expected = payload.clone();
        expected.push(b'\n');
        assert_eq!(reader.join().unwrap(), expected);
    }

    #[test]
    fn send_within_reports_a_stalled_guest() {
        let serial = Serial::new(test_eventfd());
        let payload = vec![b'a'; 200];
        // Nothing drains the FIFO: only the first 64 bytes fit.
        let ok = serial.send_within(&payload, std::time::Instant::now());
        assert!(!ok, "no reader → delivery must report failure");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ier_is_shadowed_and_replayed_to_rearm_rx_irq() {
        // Guest enables RX-data-available interrupts (IER bit 0).
        let serial = Serial::new(test_eventfd());
        serial.write(1, 0x01); // IER, DLAB=0
        let state = serial.save();
        assert_eq!(state.ier, 0x01, "IER write must be shadowed");

        // A freshly-created UART (as restore builds) has interrupts disabled, so
        // an enqueue raises no IRQ. After replaying the saved state it must.
        let irq = test_eventfd();
        let mut restored = Serial::new(irq.try_clone().unwrap());
        // Before restore: enqueue does not trigger the IRQ.
        restored.send(b"x");
        assert!(
            irq.read().is_err(),
            "fresh UART (IER=0) should not raise an RX IRQ"
        );
        // After restore: IER is re-armed, so enqueue triggers the IRQ.
        restored.restore(state);
        restored.send(b"y");
        assert!(
            irq.read().is_ok(),
            "restored UART should re-raise the RX IRQ (post-restore exec fix)"
        );
    }

    #[test]
    fn divisor_latch_writes_are_shadowed_under_dlab() {
        let serial = Serial::new(test_eventfd());
        serial.write(3, 0x80); // LCR: set DLAB
        serial.write(0, 0x0c); // DLL
        serial.write(1, 0x00); // DLM
        serial.write(3, 0x03); // LCR: clear DLAB, 8N1
        serial.write(1, 0x05); // IER (DLAB now clear)
        let s = serial.save();
        assert_eq!(s.dll, 0x0c);
        assert_eq!(s.dlm, 0x00);
        assert_eq!(s.lcr, 0x03);
        assert_eq!(s.ier, 0x05);
    }

    #[test]
    fn serial_is_send_sync() {
        assert_send_sync::<Serial>();
    }
}
