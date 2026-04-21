// upstream: src/vmm/src/vmm_config/net.rs (NetworkInterfaceConfig)
//           src/vmm/src/utils/net/mac.rs (MacAddr)
//
// Kept: `NetworkInterfaceConfig` wire struct and a MAC parser with identical
// display/serde semantics so the JSON shape is unchanged. Dropped:
// `NetBuilder`, `NetworkInterfaceUpdateConfig`, `NetworkInterfaceError`
// variants that reference the live `Net`/`TapError` device tree. Update
// endpoints aren't on the v0.3 surface.

use std::fmt;
use std::str::FromStr;

use serde::de::{Deserializer, Error};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use super::RateLimiterConfig;

/// The number of tuples (the ones separated by ":") contained in a MAC address.
pub const MAC_ADDR_LEN: u8 = 6;

/// Representation of a MAC address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct MacAddr {
    bytes: [u8; MAC_ADDR_LEN as usize],
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let b = &self.bytes;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl FromStr for MacAddr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: Vec<&str> = s.split(':').collect();
        let mut bytes = [0u8; MAC_ADDR_LEN as usize];

        if v.len() != MAC_ADDR_LEN as usize {
            return Err(String::from(s));
        }

        for i in 0..MAC_ADDR_LEN as usize {
            if v[i].len() != 2 {
                return Err(String::from(s));
            }
            bytes[i] = u8::from_str_radix(v[i], 16).map_err(|_| String::from(s))?;
        }

        Ok(MacAddr { bytes })
    }
}

impl MacAddr {
    /// Return the underlying content of this `MacAddr` in bytes.
    #[inline]
    pub fn get_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Serialize for MacAddr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serde::Serialize::serialize(&self.to_string(), serializer)
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D>(deserializer: D) -> Result<MacAddr, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <std::string::String as Deserialize>::deserialize(deserializer)?;
        MacAddr::from_str(&s).map_err(|_| D::Error::custom("The provided MAC address is invalid."))
    }
}

/// This struct represents the strongly typed equivalent of the json body from net iface
/// related requests.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInterfaceConfig {
    /// ID of the guest network interface.
    pub iface_id: String,
    /// Host level path for the guest network interface.
    pub host_dev_name: String,
    /// Guest MAC address.
    pub guest_mac: Option<MacAddr>,
    /// Rate Limiter for received packages.
    pub rx_rate_limiter: Option<RateLimiterConfig>,
    /// Rate Limiter for transmitted packages.
    pub tx_rate_limiter: Option<RateLimiterConfig>,
}
