// vmnet-probe — determines whether this machine can honor the restricted
// `com.apple.vm.networking` entitlement, which M1b (MMDS over the guest NIC
// via bridged vmnet) depends on.
//
// How to read the result:
//   * If the process is SIGKILLed on launch ("Killed: 9" / no output), AMFI
//     refused the embedded entitlement for this code signature — you need a
//     provisioning profile carrying the Virtualization Networking capability
//     (see docs/DEV_ENV.md). M1b is NOT viable with the current signing.
//   * If it prints the lines below, AMFI accepted the entitlement. A non-empty
//     bridgeable-interface list means bridged networking is usable and M1b is
//     viable on this machine.
//
// Run via `just probe-vmnet` (compiles, signs with your Apple Development
// identity + hephaestus-vmnet.entitlements, then executes).

import Foundation
import Virtualization

print("probe: launched — AMFI accepted the com.apple.vm.networking entitlement")

let interfaces = VZBridgedNetworkInterface.networkInterfaces
print("probe: bridgeable interfaces = \(interfaces.count)")
for iface in interfaces {
    let name = iface.localizedDisplayName ?? "(unnamed)"
    print("  - \(iface.identifier)  \(name)")
}

if interfaces.isEmpty {
    print("probe: RESULT ambiguous — entitlement accepted, but no bridgeable NIC found")
} else {
    print("probe: RESULT viable — bridged vmnet networking is usable; M1b can proceed")
}
