// vmnet-probe — determines whether this machine can honor the restricted
// `com.apple.vm.networking` entitlement, which M1b (MMDS over the guest NIC
// via a customizable vmnet attachment) depends on.
//
// How to read the result:
//   * If the process is SIGKILLed on launch ("Killed: 9" / no output), AMFI
//     refused the embedded entitlement for this code signature — you need a
//     provisioning profile carrying the VM Networking capability (see
//     docs/development/privileged-features.md). M1b is NOT viable with the
//     current signing.
//   * If it prints the lines below, AMFI accepted the entitlement. A non-empty
//     bridgeable-interface list means bridged networking is usable and M1b is
//     viable on this machine.
//
// Run via `just probe-vmnet` (compiles, embeds the matching provisioning
// profile in a temporary app bundle, signs it, then executes it).

import Foundation
import Virtualization
import vmnet

print("probe: launched — AMFI accepted the com.apple.vm.networking entitlement")

let interfaces = VZBridgedNetworkInterface.networkInterfaces
print("probe: bridgeable interfaces = \(interfaces.count)")
for iface in interfaces {
    let name = iface.localizedDisplayName ?? "(unnamed)"
    print("  - \(iface.identifier)  \(name)")
}

if interfaces.isEmpty {
    print("probe: bridged RESULT ambiguous — entitlement accepted, but no bridgeable NIC found")
} else {
    print("probe: bridged RESULT viable")
}

if #available(macOS 26.0, *) {
    var status: vmnet_return_t = .VMNET_FAILURE
    if let config = vmnet_network_configuration_create(.VMNET_SHARED_MODE, &status),
       let network = vmnet_network_create(config, &status),
       status == .VMNET_SUCCESS {
        _ = VZVmnetNetworkDeviceAttachment(network: network)
        print("probe: shared vmnet RESULT viable — VZVmnetNetworkDeviceAttachment created")
    } else {
        print("probe: shared vmnet RESULT failed — status \(status)")
        exit(1)
    }
} else {
    print("probe: shared vmnet RESULT unavailable — macOS 26 or later required")
    exit(1)
}
