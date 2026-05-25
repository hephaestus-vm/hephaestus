//! Firecracker HTTP API wire types and backend trait.
//!
//! This crate holds the pure-serde data types that make up the Firecracker
//! HTTP API surface, copied from upstream `vendor/firecracker/vmm/src/vmm_config/*` so the
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
/// `vendor/firecracker/vmm/src/vmm_config/*`. See the note at the top of each submodule
/// for a pointer back to the upstream file.
pub mod vmm_config;

pub use backend::{VmmBackend, VmmBackendError};
pub use vmm_config::boot_source::{BootSourceConfig, DEFAULT_KERNEL_CMDLINE};
pub use vmm_config::drive::{
    BlockDeviceConfig, BlockDeviceUpdateConfig, CacheType, FileEngineType,
};
pub use vmm_config::instance_info::{InstanceInfo, VmState};
pub use vmm_config::logger::LoggerConfig;
pub use vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
pub use vmm_config::net::NetworkInterfaceConfig;
pub use vmm_config::vm::{UpdatedVm, VmUpdatedState};
pub use vmm_config::{RateLimiterConfig, TokenBucketConfig};

#[cfg(test)]
mod property_tests {
    use super::vmm_config::boot_source::BootSourceConfig;
    use super::vmm_config::drive::{BlockDeviceConfig, BlockDeviceUpdateConfig};
    use super::vmm_config::logger::LoggerConfig;
    use super::vmm_config::machine_config::{MachineConfig, MachineConfigUpdate};
    use super::vmm_config::metrics::MetricsConfig;
    use super::vmm_config::mmds::MmdsConfig;
    use super::vmm_config::net::NetworkInterfaceConfig;
    use super::vmm_config::snapshot::{CreateSnapshotParams, LoadSnapshotConfig};
    use super::vmm_config::vm::UpdatedVm;
    use super::vmm_config::vsock::VsockConfig;
    use proptest::prelude::*;
    use serde::de::DeserializeOwned;
    use serde_json::{Map, Number, Value};

    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| Value::Number(Number::from(n))),
            ".{0,64}".prop_map(Value::String),
        ];

        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                prop::collection::btree_map("[A-Za-z_][A-Za-z0-9_]{0,16}", inner, 0..8)
                    .prop_map(|map| Value::Object(map.into_iter().collect::<Map<_, _>>())),
            ]
        })
    }

    fn parse_is_clean<T: DeserializeOwned>(value: &Value) {
        let _ = serde_json::from_value::<T>(value.clone()).map_err(|err| err.to_string());
    }

    proptest! {
        #[test]
        fn random_json_never_panics_wire_deserializers(value in json_value()) {
            parse_is_clean::<BootSourceConfig>(&value);
            parse_is_clean::<BlockDeviceConfig>(&value);
            parse_is_clean::<BlockDeviceUpdateConfig>(&value);
            parse_is_clean::<MachineConfig>(&value);
            parse_is_clean::<MachineConfigUpdate>(&value);
            parse_is_clean::<NetworkInterfaceConfig>(&value);
            parse_is_clean::<LoggerConfig>(&value);
            parse_is_clean::<MetricsConfig>(&value);
            parse_is_clean::<MmdsConfig>(&value);
            parse_is_clean::<VsockConfig>(&value);
            parse_is_clean::<CreateSnapshotParams>(&value);
            parse_is_clean::<LoadSnapshotConfig>(&value);
            parse_is_clean::<UpdatedVm>(&value);
        }
    }
}
