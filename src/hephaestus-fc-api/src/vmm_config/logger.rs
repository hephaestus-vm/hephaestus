// upstream: vendor/firecracker/vmm/src/logger/logging.rs (`LoggerConfig`, `LevelFilter`)
//
// Just the wire struct. Upstream parses `level` as a `LevelFilter` enum
// with case-insensitive backwards-compat deserialization (Warning/WARN/
// etc. all map to Warn); we accept it as an opaque String and let the
// backend decide what to do with it. The other three fields match
// upstream verbatim.
//
// Wire-compat note: upstream's `LevelFilter` enum uses `#[serde(...)]`
// tweaks to accept multiple spellings. Pinning to a String here means
// hephaestus-firecracker won't reject spellings upstream would accept.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Wire struct for PUT `/logger`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoggerConfig {
    /// Named pipe or file used as output for logs.
    pub log_path: Option<PathBuf>,
    /// The log level (Off/Trace/Debug/Info/Warn/Error, case-insensitive).
    pub level: Option<String>,
    /// Whether to show the log level in the log.
    pub show_level: Option<bool>,
    /// Whether to show the log origin (module + line) in the log.
    pub show_log_origin: Option<bool>,
    /// Module filter (e.g. `vmm::vstate`). Messages from outside this
    /// prefix are dropped. Upstream parses this; we accept-and-ignore.
    pub module: Option<String>,
}
