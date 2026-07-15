import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import fc_compat_net_e2e


class FcCompatNetE2eTests(unittest.TestCase):
    def test_api_configs_preserve_paths_and_network_identity(self) -> None:
        self.assertEqual(
            fc_compat_net_e2e.vsock_config("/tmp/guest socket"),
            {"guest_cid": 3, "uds_path": "/tmp/guest socket"},
        )
        self.assertEqual(
            fc_compat_net_e2e.network_config(),
            {
                "iface_id": "eth0",
                "host_dev_name": "tap0",
                "guest_mac": "AA:FC:00:00:00:01",
            },
        )
        self.assertEqual(
            fc_compat_net_e2e.boot_config("/kernel path", "/initrd path"),
            {
                "kernel_image_path": "/kernel path",
                "initrd_path": "/initrd path",
                "boot_args": "console=hvc0 rdinit=/init quiet loglevel=3",
            },
        )
        self.assertEqual(
            fc_compat_net_e2e.drive_config("/rootfs path"),
            {
                "drive_id": "rootfs",
                "path_on_host": "/rootfs path",
                "is_root_device": True,
                "is_read_only": False,
            },
        )

    def test_base_guest_command_only_requires_a_network_device(self) -> None:
        command = fc_compat_net_e2e.guest_command(False)
        self.assertIn(b"/sys/class/net", command)
        self.assertNotIn(b"169.254.169.254", command)

    def test_mmds_guest_command_configures_dhcp_and_fetches_metadata(self) -> None:
        command = fc_compat_net_e2e.guest_command(True)
        self.assertIn(b"udhcpc", command)
        self.assertIn(b"http://169.254.169.254/latest/meta-data/instance-id", command)
        self.assertIn(b"i-hephaestus-vmnet", command)


if __name__ == "__main__":
    unittest.main()
