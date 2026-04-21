// upstream: src/vmm/src/vmm_config/mod.rs
//
// Copied the public-facing (wire) portion — `TokenBucketConfig` and
// `RateLimiterConfig` — and dropped the conversions to live
// `RateLimiter`/`TokenBucket` objects (those require the upstream
// `rate_limiter` module which is Linux-only via `timerfd`).

use serde::{Deserialize, Serialize};

/// Kernel + initrd + cmdline wire struct (`PUT /boot-source`).
pub mod boot_source;
/// Block device wire struct (`PUT /drives/{id}`).
pub mod drive;
/// Instance info wire struct (`GET /`).
pub mod instance_info;
/// Host-side logger config wire struct (`PUT /logger`).
pub mod logger;
/// Machine config wire struct (`GET`/`PUT`/`PATCH /machine-config`).
pub mod machine_config;
/// Network interface wire struct (`PUT /network-interfaces/{id}`).
pub mod net;
/// VM state update wire struct (`PATCH /vm`).
pub mod vm;

/// A public-facing, stateless structure, holding all the data we need to create a TokenBucket
/// (live) object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenBucketConfig {
    /// See TokenBucket::size.
    pub size: u64,
    /// See TokenBucket::one_time_burst.
    pub one_time_burst: Option<u64>,
    /// See TokenBucket::refill_time.
    pub refill_time: u64,
}

/// A public-facing, stateless structure, holding all the data we need to create a RateLimiter
/// (live) object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimiterConfig {
    /// Data used to initialize the RateLimiter::bandwidth bucket.
    pub bandwidth: Option<TokenBucketConfig>,
    /// Data used to initialize the RateLimiter::ops bucket.
    pub ops: Option<TokenBucketConfig>,
}
