// upstream: vendor/firecracker/vmm/src/vmm_config/snapshot.rs
//
// Wire structs for `PUT /snapshot/create` and `PUT /snapshot/load`.
// Trimmed from upstream to drop the Linux-only `MemBackendType::Uffd`
// (UFFD is a Linux feature; macOS has no equivalent), and to inline the
// internal `LoadSnapshotParams` since we use the user-facing
// `LoadSnapshotConfig` shape directly.
//
// Wire-compat divergence (the "A+stub" decision; see
// `docs/hephaestus-progress.md`): `mem_file_path` is accepted but VZ's
// `saveMachineStateTo:` writes one combined blob, not separate state
// and memory files. We honor `mem_file_path` only by touching an empty
// stub file so existence checks pass; the real save lives at
// `snapshot_path`. `mem_backend.backend_type=Uffd` is rejected as
// `NotSupported`. Cross-tool snapshot interop with real Firecracker is
// fundamentally impossible — the underlying hypervisors produce
// structurally different artifacts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Snapshot type. Upstream supports `Diff` (incremental); VZ has no
/// equivalent (no dirty-page tracking primitive exposed), so the
/// backend rejects `Diff` with `NotSupported`. `Full` is what real
/// clients use anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum SnapshotType {
    /// Diff snapshot — not supported on macOS VZ.
    Diff,
    /// Full snapshot.
    #[default]
    Full,
}

/// Memory backend kind. UFFD is Linux-only (page-fault userland
/// handler); we accept the enum so wire shapes parse, but the backend
/// rejects anything other than `File`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum MemBackendType {
    /// Memory comes from a file on disk.
    #[default]
    File,
    /// UFFD-backed memory (Linux-only). Rejected by the backend.
    Uffd,
}

/// Body of `PUT /snapshot/create`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSnapshotParams {
    /// File the microVM state blob is written to. (On VZ this also
    /// contains the memory contents — see the module-level note.)
    pub snapshot_path: PathBuf,
    /// File the guest memory would be written to under upstream
    /// semantics. Honored by touching an empty stub at this path.
    pub mem_file_path: PathBuf,
    /// `Full` is the only supported value. `Diff` is rejected.
    #[serde(default)]
    pub snapshot_type: SnapshotType,
    /// Snapshot format version requested by the client. Upstream uses
    /// this for cross-version compat; we have one format and ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Memory-backend descriptor for `LoadSnapshotConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemBackendConfig {
    /// Where the backend (file path or UDS) lives.
    pub backend_path: PathBuf,
    /// Backend kind. Anything other than `File` is rejected.
    pub backend_type: MemBackendType,
}

/// Body of `PUT /snapshot/load`. Mirrors upstream's
/// `LoadSnapshotConfig` so `firectl`/Kata-shaped JSON parses cleanly.
/// `mem_backend` and `mem_file_path` are alternatives in upstream;
/// either supplies the memory source. We accept either (or both) and
/// ignore the actual contents — see module note.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSnapshotConfig {
    /// File the microVM state was saved to.
    pub snapshot_path: PathBuf,
    /// Optional memory file path (alternative to `mem_backend`).
    /// Accepted; ignored on restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_file_path: Option<PathBuf>,
    /// Optional memory backend descriptor (alternative to
    /// `mem_file_path`). Accepted; backend kind validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_backend: Option<MemBackendConfig>,
    /// Diff-snapshot enable flag. Rejected if true.
    #[serde(default)]
    pub enable_diff_snapshots: bool,
    /// Dirty-page tracking flag. Rejected if true.
    #[serde(default)]
    pub track_dirty_pages: bool,
    /// When true, the microVM is resumed immediately after load.
    /// When false, it stays Paused and the client must `PATCH /vm
    /// {state: Resumed}` to start it.
    #[serde(default)]
    pub resume_vm: bool,
}
