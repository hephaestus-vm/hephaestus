//! Firecracker HTTP API wire types and backend trait.
//!
//! This crate holds the pure-serde data types that make up the Firecracker
//! HTTP API surface, copied from upstream `src/vmm/src/vmm_config/*` so the
//! macOS port can consume them without the Linux-only VMM tree (`kvm-*`,
//! `vhost`, `memfd`, `userfaultfd`, `micro_http`'s epoll server loop, etc).
//!
//! Each module carries a `// upstream:` pointer back at the file it was
//! lifted from. Drop-in compat depends on the wire shapes staying aligned
//! with upstream; treat divergence as a bug and fold upstream config-struct
//! changes down into this crate when we rebase.
//!
//! The [`VmmBackend`] trait is how `hephaestus-firecracker` calls into a
//! concrete VMM implementation (our Virtualization.framework-backed one, or
//! any future alternative). It is *not* an upstream concept — upstream
//! hard-codes the Linux `Vmm` struct at the HTTP handler boundary.

#![warn(missing_docs)]

pub mod backend;
/// Wire structs for the Firecracker HTTP API, copied from upstream
/// `src/vmm/src/vmm_config/*`. See the note at the top of each submodule
/// for a pointer back to the upstream file.
pub mod vmm_config;

pub use backend::{VmmBackend, VmmBackendError};
pub use vmm_config::boot_source::{BootSourceConfig, DEFAULT_KERNEL_CMDLINE};
pub use vmm_config::drive::{BlockDeviceConfig, CacheType, FileEngineType};
pub use vmm_config::instance_info::{InstanceInfo, VmState};
pub use vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
pub use vmm_config::net::NetworkInterfaceConfig;
pub use vmm_config::{RateLimiterConfig, TokenBucketConfig};
