//! Shared Tarit VMM wire protocol types.

pub mod api;
pub mod config;
pub mod integrity;
pub mod pty;
pub mod state;

pub use api::{
    ApiRequest, ApiResponse, GuestNetworkRepair, LiveSnapshotStats, LiveSnapshotTermination,
    MemoryHotplugStatus, MemoryIntegrity, RestoreMemoryPolicy, ScratchIdentity, VmSpec,
    MAX_API_FRAME_LEN, MAX_MEMORY_HOTPLUG_WAIT_MS, RETRYABLE_ERROR_PREFIX,
};
pub use config::{
    KernelConfig, MemoryConfig, NetConfig, PortForwardConfig, VcpuConfig, VmConfig, VolumeConfig,
};
pub use integrity::{ArtifactIntegrity, ArtifactKind, IntegrityManifest, INTEGRITY_CHUNK_SIZE};
pub use pty::{
    read_frame, write_error_frame, write_frame, write_json_frame, PtyExit, PtyResize, PtyStart,
    PtyStreamFrame, MAX_FRAME_LEN, TYPE_DATA, TYPE_ERROR, TYPE_EXEC_START, TYPE_EXIT, TYPE_RESIZE,
    TYPE_START, TYPE_STDERR, TYPE_STDIN_EOF,
};
pub use state::{VmState, VmStatus};
