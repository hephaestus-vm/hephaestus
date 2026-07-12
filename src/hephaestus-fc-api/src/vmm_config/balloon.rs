// upstream: firecracker-go-sdk/client/models/balloon.go + balloon_update.go
//
// Firecracker's virtio-balloon reclaims guest memory: inflating the balloon by
// `amount_mib` hands that many MiB back to the host. hephaestus maps this onto
// VZ's traditional memory balloon (`targetVirtualMachineMemorySize`), where the
// target is the memory the guest should keep — so a reclaim of `amount_mib`
// sets the target to `mem_size_mib - amount_mib`.

use serde::{Deserialize, Serialize};

/// Wire struct for `PUT /balloon` (and returned by `GET /balloon`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalloonDeviceConfig {
    /// Target amount of memory (MiB) to reclaim from the guest by inflating
    /// the balloon.
    pub amount_mib: u32,
    /// Whether the balloon deflates to relieve guest OOM. Accepted for wire
    /// compatibility; VZ's traditional balloon manages this itself.
    #[serde(default)]
    pub deflate_on_oom: bool,
    /// Balloon statistics polling interval (seconds). VZ exposes no balloon
    /// statistics, so a non-zero value is rejected.
    #[serde(default)]
    pub stats_polling_interval_s: u32,
}

/// Wire struct for `PATCH /balloon` — adjust the reclaim target at runtime.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BalloonUpdateConfig {
    /// New target amount of memory (MiB) to reclaim from the guest.
    pub amount_mib: u32,
}
