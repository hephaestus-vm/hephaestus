// upstream: vendor/firecracker/vmm/src/vmm_config/metrics.rs
//
// Wire shape only. Upstream's `Metrics::init` opens the path and starts a
// periodic flush loop publishing ~30 fields of KVM/vhost/vsock counters;
// most of those don't have macOS analogues. hephaestus-firecracker accepts
// the path and writes newline-delimited JSON with Firecracker-compatible
// top-level groups, zeros for Linux-only counters, and a `hephaestus` object
// for macOS/VZ-specific counters. `metrics_path` is the only field upstream
// requires; a `level` knob exists in some downstreams but not stock.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Wire struct for `PUT /metrics`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Named pipe or file to which metrics are flushed.
    pub metrics_path: PathBuf,
}
