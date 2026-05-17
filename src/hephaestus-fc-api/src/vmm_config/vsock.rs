// upstream: firecracker-go-sdk/client/models/vsock.go
//
// Firecracker exposes a single virtio-vsock device backed by a host UDS.
// Host-initiated clients connect to `uds_path`, send `CONNECT <PORT>\n`, and
// Firecracker bridges the stream to the guest vsock port.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Wire struct for `PUT /vsock`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VsockConfig {
    /// Guest vsock CID. Firecracker requires this to be >= 3. VZ assigns the
    /// CID internally, so hephaestus validates/stores the field but cannot
    /// force the exact guest CID.
    pub guest_cid: u32,
    /// Path to the host UNIX domain socket used to proxy vsock connections.
    pub uds_path: PathBuf,
    /// Deprecated Firecracker field; accepted for compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vsock_id: Option<String>,
}
