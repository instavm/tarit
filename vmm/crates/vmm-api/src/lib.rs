//! vmm-api: length-prefixed JSON control protocol over a Unix Domain Socket.
//!
//! Each request and response is framed as `[4-byte big-endian length][JSON body]`.
//! See [`rpc`] for framing helpers and [`types`] for the stable request/response
//! schema consumed by the orchestrator.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod rpc;
pub mod types;

pub use types::{ApiRequest, ApiResponse, VmSpec};
