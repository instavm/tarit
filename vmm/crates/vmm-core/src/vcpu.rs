//! vCPU abstraction.
//!
//! On Linux+KVM (feature `kvm`) this wraps a `kvm_ioctls::Vcpu` and runs
//! `KVM_RUN` in a dedicated thread. Off-KVM, only the type exists so the rest
//! of the VMM compiles.

use crate::error::Result;
/// Per-vCPU identifier (0..vcpu_count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpuId(pub u8);

impl VcpuId {
    /// OS thread name of this vCPU's run loop: `vcpu N`. Host tooling finds
    /// vCPU threads by name (for example to place only them in a cgroup v2
    /// threaded child), as with Firecracker's `fc_vcpu N`. Linux keeps at
    /// most 15 bytes of a thread name; the longest, `vcpu 255`, is 8.
    pub fn thread_name(self) -> String {
        format!("vcpu {}", self.0)
    }

    /// Spawn this vCPU's run-loop thread, named [`Self::thread_name`].
    pub fn spawn_thread<F, T>(self, run: F) -> std::io::Result<std::thread::JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        std::thread::Builder::new()
            .name(self.thread_name())
            .spawn(run)
    }
}

/// Keep interrupting a stopping vCPU until it has actually exited. A single
/// signal can be handled between the stop-flag check and KVM_RUN entry, or
/// before the worker has published its TID. The caller must set stop first.
/// Keep ownership and join afterwards: detaching could release live VM memory.
#[cfg(any(
    test,
    all(feature = "kvm", target_arch = "x86_64", target_os = "linux")
))]
pub(crate) fn wait_for_vcpu_exit<T>(
    handle: &std::thread::JoinHandle<T>,
    mut alive: impl FnMut() -> bool,
    mut kick: impl FnMut(),
) {
    // A seccomp-killed worker cannot publish Rust's completion packet. The
    // OS liveness check permits the caller's abnormal-exit join handling.
    while !handle.is_finished() && alive() {
        kick();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// A vCPU handle. On KVM this owns the `kvm_ioctls::Vcpu`; the run loop lives
/// in `Vcpu::run`. Off-KVM the body is a no-op stub.
pub struct Vcpu {
    pub id: VcpuId,
    #[cfg(feature = "kvm")]
    pub kvm_cpu: kvm_ioctls::VcpuFd,
}

impl Vcpu {
    #[cfg(feature = "kvm")]
    pub fn new(id: VcpuId, vm: &kvm_ioctls::VmFd) -> Result<Self> {
        let kvm_cpu = vm
            .create_vcpu(id.0 as u64)
            .map_err(|e| crate::error::VmmError::Kvm(format!("create_vcpu({}): {e}", id.0)))?;
        Ok(Self { id, kvm_cpu })
    }

    #[cfg(not(feature = "kvm"))]
    pub fn new(id: VcpuId) -> Result<Self> {
        Ok(Self { id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vcpu_thread_names_fit_the_kernel_comm_limit() {
        assert_eq!(VcpuId(0).thread_name(), "vcpu 0");
        assert_eq!(VcpuId(17).thread_name(), "vcpu 17");
        for id in 0..=u8::MAX {
            // TASK_COMM_LEN is 16 including the terminating NUL.
            assert!(VcpuId(id).thread_name().len() <= 15);
        }
    }

    #[test]
    fn stopping_vcpu_retries_a_lost_kick_and_unpublished_tid() {
        for missed_kicks in [1, 3] {
            let (send, recv) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || recv.recv().unwrap());
            let mut kicks = 0;
            wait_for_vcpu_exit(
                &worker,
                || true,
                || {
                    kicks += 1;
                    assert!(kicks < 1000, "worker did not stop within retry budget");
                    // Model a signal handled before KVM_RUN, or a TID not yet
                    // published: those attempts cannot wake the blocked worker.
                    if kicks > missed_kicks {
                        let _ = send.send(42);
                    }
                },
            );
            assert_eq!(worker.join().unwrap(), 42);
            assert!(kicks > missed_kicks);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vcpu_thread_carries_its_name_in_the_kernel() {
        let comm = VcpuId(7)
            .spawn_thread(|| std::fs::read_to_string("/proc/thread-self/comm").unwrap())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(comm, "vcpu 7\n");
    }
}
