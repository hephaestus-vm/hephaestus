// upstream: vendor/firecracker/vmm/src/vmm_config/snapshot.rs (`Vm` + `VmState`)
//
// PATCH /vm body: `{"state": "Paused"}` / `{"state": "Resumed"}`. Renamed
// the Rust types to `UpdatedVm` + `VmUpdatedState` to avoid a collision
// with `instance_info::VmState` (`NotStarted`/`Running`/`Paused`) which
// is also in this crate. Wire shape is unchanged — serde sees the same
// `{"state": "..."}` JSON.

use serde::{Deserialize, Serialize};

/// Target state for a PATCH `/vm` request.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum VmUpdatedState {
    /// Pause the running microVM.
    Paused,
    /// Resume a paused microVM.
    Resumed,
}

/// Wire struct for PATCH `/vm`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatedVm {
    /// The microVM state, which can be `Paused` or `Resumed`.
    pub state: VmUpdatedState,
}
