//! Shared Tarit VMM wire protocol types.

pub mod api;
pub mod config;
pub mod pty;
pub mod state;

pub use api::{ApiRequest, ApiResponse, VmSpec, MAX_API_FRAME_LEN};
pub use config::{
    KernelConfig, MemoryConfig, NetConfig, PortForwardConfig, VcpuConfig, VmConfig, VolumeConfig,
};
pub use pty::{
    read_frame, write_error_frame, write_frame, write_json_frame, PtyExit, PtyResize, PtyStart,
    PtyStreamFrame, MAX_FRAME_LEN, TYPE_DATA, TYPE_ERROR, TYPE_EXIT, TYPE_RESIZE, TYPE_START,
};
pub use state::{VmState, VmStatus};
