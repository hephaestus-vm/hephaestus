import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import fc_compat_vsock_e2e


class FcCompatVsockE2eTests(unittest.TestCase):
    def test_api_configs_preserve_paths(self) -> None:
        self.assertEqual(
            fc_compat_vsock_e2e.logger_config("/tmp/log path"),
            {
                "log_path": "/tmp/log path",
                "level": "Debug",
                "show_level": True,
                "show_log_origin": True,
            },
        )
        self.assertEqual(
            fc_compat_vsock_e2e.metrics_config("/tmp/metrics path"),
            {"metrics_path": "/tmp/metrics path"},
        )
        self.assertEqual(
            fc_compat_vsock_e2e.vsock_config("/tmp/vsock path"),
            {"guest_cid": 3, "uds_path": "/tmp/vsock path"},
        )
        self.assertEqual(
            fc_compat_vsock_e2e.boot_config("/kernel path", "/initrd path"),
            {
                "kernel_image_path": "/kernel path",
                "initrd_path": "/initrd path",
                "boot_args": "console=hvc0 rdinit=/init quiet loglevel=3",
            },
        )
        self.assertEqual(
            fc_compat_vsock_e2e.drive_config("/rootfs path"),
            {
                "drive_id": "rootfs",
                "path_on_host": "/rootfs path",
                "is_root_device": True,
                "is_read_only": False,
            },
        )

    def test_guest_command_carries_mmds_and_echo_expectations(self) -> None:
        command = fc_compat_vsock_e2e.guest_command()
        self.assertIn(fc_compat_vsock_e2e.INSTANCE_ID.encode(), command)
        self.assertIn(str(fc_compat_vsock_e2e.ECHO_PORT).encode(), command)
        self.assertIn(fc_compat_vsock_e2e.ECHO_TOKEN, command)


if __name__ == "__main__":
    unittest.main()
