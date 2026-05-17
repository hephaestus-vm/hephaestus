// upstream: vendor/firecracker/firecracker/src/api_server/parsed_request.rs
//
// Wire response for `GET /version`. The value intentionally tracks the
// upstream Firecracker API snapshot hephaestus claims compatibility with,
// not the hephaestus crate version.

use serde::Serialize;

/// Upstream Firecracker version whose HTTP API wire surface this release
/// targets. Bump only after syncing `hephaestus-fc-api` wire structs against
/// the corresponding upstream tree and running the compat harness.
pub const FIRECRACKER_COMPAT_VERSION: &str = "1.16.0-dev";

/// Describes the Firecracker compatibility version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FirecrackerVersion {
    /// Firecracker build/API version reported to compatibility clients.
    pub firecracker_version: &'static str,
}

impl Default for FirecrackerVersion {
    fn default() -> Self {
        Self {
            firecracker_version: FIRECRACKER_COMPAT_VERSION,
        }
    }
}
