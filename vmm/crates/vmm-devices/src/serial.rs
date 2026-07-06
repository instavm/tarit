//! 16550 serial console via `vm-superio`.

use crate::persist::Persist;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use vm_superio::serial::{NoEvents, Serial as VmSerial};
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

pub struct EventFdTrigger(EventFd);

impl EventFdTrigger {
    pub fn new(evt: EventFd) -> Self {
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
            buf.extend_from_slice(bytes);
            let len = buf.len();
            if len > MAX_OUTPUT {
                buf.drain(0..len - MAX_OUTPUT);
            }
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
/// `vm-superio` 0.8 exposes no register getters and recreates a fresh UART with
/// all registers zeroed, so a restored device would have interrupts *disabled*
/// even though the running guest had enabled them. Host→guest bytes (e.g. an
/// `exec` command) would then raise no RX interrupt and the guest agent, blocked
/// in `read(/dev/ttyS0)`, would never wake — exec hangs until timeout even
/// though the guest itself is live (TX/console still works). We therefore shadow
/// every guest write to a writable register and replay it on restore.
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

/// A 16550 UART backed by an EventFd IRQ trigger and a captured stdout sink.
pub struct Serial {
    inner: Mutex<VmSerial<EventFdTrigger, NoEvents, SerialOut>>,
    out_buf: Arc<Mutex<Vec<u8>>>,
    /// Shadow of the guest-programmed writable registers, updated on every
    /// `write`, so the UART configuration can be snapshotted and replayed
    /// (`vm-superio` exposes no register getters).
    shadow: Mutex<SerialState>,
}

impl Serial {
    pub fn new(irq_evt: EventFd) -> Self {
        let out_buf = Arc::new(Mutex::new(Vec::new()));
        let out = SerialOut {
            buf: out_buf.clone(),
        };
        Self {
            inner: Mutex::new(VmSerial::new(EventFdTrigger::new(irq_evt), out)),
            out_buf,
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

    pub fn send(&self, bytes: &[u8]) {
        let mut serial = self.inner.lock().unwrap();
        let _ = serial.enqueue_raw_bytes(bytes);
        let _ = serial.enqueue_raw_bytes(b"\n");
    }

    pub fn drain_output(&self) -> Vec<u8> {
        let mut buf = self.out_buf.lock().unwrap();
        std::mem::take(&mut *buf)
    }
}

impl Persist for Serial {
    type State = SerialState;

    fn save(&self) -> Self::State {
        self.shadow.lock().unwrap().clone()
    }

    fn restore(&mut self, state: Self::State) {
        // Replay the guest-programmed registers onto the fresh UART, in the
        // order the guest would: program the baud divisor under DLAB=1, then
        // restore the real LCR (which sets the final DLAB), then IER/FCR/MCR/SCR.
        // Re-applying IER is what re-arms the RX interrupt so post-restore
        // host→guest bytes reach the guest agent.
        {
            let inner = self.inner.get_mut().unwrap();
            let _ = inner.write(3, state.lcr | 0x80); // DLAB=1 to reach the latch
            let _ = inner.write(0, state.dll);
            let _ = inner.write(1, state.dlm);
            let _ = inner.write(3, state.lcr); // final LCR (guest's DLAB)
            let _ = inner.write(2, state.fcr);
            let _ = inner.write(4, state.mcr);
            let _ = inner.write(7, state.scr);
            // IER last, and only meaningful with DLAB=0; if the guest left DLAB
            // set (unusual), offset 1 is the divisor high byte we already wrote.
            if state.lcr & 0x80 == 0 {
                let _ = inner.write(1, state.ier);
            }
        }
        *self.shadow.get_mut().unwrap() = state;
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
