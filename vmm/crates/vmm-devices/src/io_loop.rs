//! Event-manager I/O loop — the epoll-driven device service thread
//! (PRD §4: "one device/event thread (epoll via `event-manager`)").
//!
//! Linux-only (epoll). On other hosts the type exists as a stub.
//!
//! This helper records device fds (tap, virtqueue kicks, eventfds) with tokens
//! for controller wiring. Device-specific Linux data planes live in the
//! virtio `*_io_loop` modules.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IoLoopError {
    #[error("epoll: {0}")]
    Epoll(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// An event source the I/O loop watches: a raw fd + the data identifying
/// which device/queue owns it.
#[derive(Debug, Clone, Copy)]
pub struct EventSource {
    pub fd: i32,
    pub token: u64,
}

/// The device I/O loop. Linux: epoll. Non-Linux: stub.
pub struct IoLoop {
    /// The epoll fd reserved for Linux implementations that embed this helper.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    epoll_fd: Option<i32>,
    sources: Vec<EventSource>,
}

impl IoLoop {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            epoll_fd: None,
            sources: Vec::new(),
        }
    }

    /// Register an event source (a device fd + a token identifying its owner).
    pub fn add(&mut self, src: EventSource) {
        self.sources.push(src);
    }

    pub fn sources(&self) -> &[EventSource] {
        &self.sources
    }
}

impl Default for IoLoop {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_loop_has_no_sources() {
        let l = IoLoop::new();
        assert!(l.sources().is_empty());
    }

    #[test]
    fn add_registers_source() {
        let mut l = IoLoop::new();
        l.add(EventSource { fd: 5, token: 100 });
        l.add(EventSource { fd: 6, token: 200 });
        assert_eq!(l.sources().len(), 2);
        assert_eq!(l.sources()[0].fd, 5);
        assert_eq!(l.sources()[1].token, 200);
    }

    #[test]
    fn default_is_empty() {
        let l = IoLoop::default();
        assert!(l.sources().is_empty());
    }

    #[test]
    fn event_source_is_copy() {
        let s = EventSource { fd: 7, token: 42 };
        let s2 = s;
        assert_eq!(s.fd, s2.fd);
        assert_eq!(s.token, s2.token);
    }
}
