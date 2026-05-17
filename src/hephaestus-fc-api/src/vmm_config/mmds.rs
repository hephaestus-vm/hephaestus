// upstream: vendor/firecracker/vmm/src/resources.rs (`MmdsConfig`) and
// vendor/firecracker/vmm/src/mmds/data_store.rs
//
// hephaestus currently implements MMDS as API-level storage only: the HTTP
// control plane accepts/stores/returns JSON so orchestrators can configure
// metadata without tripping over missing routes. Guest-visible delivery is a
// later vsock-backed feature.

use serde::{Deserialize, Serialize};

/// Enumeration indicating the MMDS version to be configured.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum MmdsVersion {
    /// MMDS version 1.
    #[default]
    V1,
    /// MMDS version 2.
    V2,
}

/// Defines the MMDS configuration (`PUT /mmds/config`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MmdsConfig {
    /// A valid IPv4 link-local address. Accepted for wire compatibility;
    /// not currently bound to a guest network path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4_address: Option<String>,
    /// Network interface IDs allowed to forward MMDS requests upstream.
    /// Required by the Firecracker swagger; hephaestus stores but does not
    /// enforce the binding in the API-only implementation.
    pub network_interfaces: Vec<String>,
    /// MMDS protocol version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<MmdsVersion>,
}

impl Default for MmdsConfig {
    fn default() -> Self {
        Self {
            ipv4_address: None,
            network_interfaces: Vec::new(),
            version: Some(MmdsVersion::V1),
        }
    }
}
