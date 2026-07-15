import datetime
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import vmnet_profile


class VmnetProfileTests(unittest.TestCase):
    def setUp(self) -> None:
        self.now = datetime.datetime(2026, 7, 15, tzinfo=datetime.timezone.utc)
        self.profile: vmnet_profile.Profile = {
            "Platform": ["OSX"],
            "ExpirationDate": self.now + datetime.timedelta(days=30),
            "Entitlements": {
                "com.apple.application-identifier": (
                    "XS9395WADZ.ca.nodegroup.hephaestus"
                ),
                "com.apple.developer.team-identifier": "XS9395WADZ",
                "com.apple.vm.networking": True,
                "keychain-access-groups": ["XS9395WADZ.*"],
            },
        }

    def test_matching_profile_requires_bundle_capability_platform_and_expiry(self) -> None:
        self.assertTrue(
            vmnet_profile.profile_matches(
                self.profile, "ca.nodegroup.hephaestus", self.now
            )
        )

        cases = (
            ("ca.nodegroup.other", self.profile),
            (
                "ca.nodegroup.hephaestus",
                {**self.profile, "Platform": ["iOS"]},
            ),
            (
                "ca.nodegroup.hephaestus",
                {**self.profile, "ExpirationDate": self.now},
            ),
            (
                "ca.nodegroup.hephaestus",
                {
                    **self.profile,
                    "Entitlements": {
                        **self.profile["Entitlements"],
                        "com.apple.vm.networking": False,
                    },
                },
            ),
        )
        for bundle_id, profile in cases:
            with self.subTest(bundle_id=bundle_id, profile=profile):
                self.assertFalse(
                    vmnet_profile.profile_matches(profile, bundle_id, self.now)
                )

    def test_signing_entitlements_keep_only_required_values(self) -> None:
        self.assertEqual(
            vmnet_profile.signing_entitlements(self.profile),
            {
                "com.apple.application-identifier": (
                    "XS9395WADZ.ca.nodegroup.hephaestus"
                ),
                "com.apple.developer.team-identifier": "XS9395WADZ",
                "com.apple.security.virtualization": True,
                "com.apple.vm.networking": True,
            },
        )

    def test_app_info_describes_a_background_application(self) -> None:
        self.assertEqual(
            vmnet_profile.app_info("ca.nodegroup.hephaestus", "firecracker"),
            {
                "CFBundleExecutable": "firecracker",
                "CFBundleIdentifier": "ca.nodegroup.hephaestus",
                "CFBundleName": "Hephaestus",
                "CFBundlePackageType": "APPL",
                "CFBundleShortVersionString": "1.0",
                "CFBundleVersion": "1",
                "LSBackgroundOnly": True,
            },
        )


if __name__ == "__main__":
    unittest.main()
