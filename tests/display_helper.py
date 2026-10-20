#!/usr/bin/env python3
"""Exercise the privileged DRM helper against an isolated synthetic sysfs tree."""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "contrib" / "virtual-desktop" / "uu-display-helper.c"


class SyntheticDrm:
    def __init__(self, parent, cards):
        self.parent = pathlib.Path(parent)
        self.root = self.parent / "sys"
        self.state = self.parent / "state"
        self.root.mkdir(mode=0o755)
        (self.root / ".uu-display-helper-fixture").write_text("synthetic-v1\n")
        os.chmod(self.root / ".uu-display-helper-fixture", 0o600)
        (self.root / "class" / "drm").mkdir(parents=True)
        (self.root / "devices").mkdir()
        (self.root / "bus" / "pci" / "drivers").mkdir(parents=True)
        (self.root / "kernel" / "debug" / "dri").mkdir(parents=True)
        for card_id, driver_name, connectors in cards:
            self.add_card(card_id, driver_name, connectors)

    def add_card(self, card_id, driver_name, connectors):
        driver = self.root / "bus" / "pci" / "drivers" / driver_name
        driver.mkdir(exist_ok=True)
        device = self.root / "devices" / f"0000:00:{card_id:02x}.0"
        card = device / "drm" / f"card{card_id}"
        (card / "device").mkdir(parents=True)
        (card / "device" / "driver").symlink_to(driver)
        (self.root / "class" / "drm" / f"card{card_id}").symlink_to(card)
        for kind, status in connectors:
            name = f"card{card_id}-{kind}"
            connector = card / name
            connector.mkdir()
            status_file = connector / "status"
            status_file.write_text(status + "\n")
            os.chmod(status_file, 0o644)
            (self.root / "class" / "drm" / name).symlink_to(connector)
            force_file = self.root / "kernel" / "debug" / "dri" / str(card_id) / kind / "force"
            force_file.parent.mkdir(parents=True, exist_ok=True)
            force_file.write_text("unspecified\n")
            os.chmod(force_file, 0o644)

    def status_path(self, name):
        return self.root / "devices" / "0000:00:00.0" / "drm" / "card0" / name / "status"

    def set_status(self, name, status):
        self.status_path(name).write_text(status + "\n")

    def get_status(self, name):
        return self.status_path(name).read_text().strip()

    def force_path(self, name):
        card, kind = name.split("-", 1)
        card_id = int(card[4:])
        return self.root / "kernel" / "debug" / "dri" / str(card_id) / kind / "force"

    def set_force(self, name, force):
        self.force_path(name).write_text(force + "\n")

    def get_force(self, name):
        return self.force_path(name).read_text().strip()


class DisplayHelperTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.name != "posix":
            raise unittest.SkipTest("helper fixture requires POSIX sysfs semantics")
        compiler = shutil.which("cc")
        if not compiler:
            raise RuntimeError("a C compiler is required for the display helper fixture")
        cls.build_dir = tempfile.TemporaryDirectory(prefix="uu-display-helper-build-")
        cls.test_binary = pathlib.Path(cls.build_dir.name) / "uu-display-helper-test"
        cls.prod_binary = pathlib.Path(cls.build_dir.name) / "uu-display-helper-prod"
        common = [
            compiler,
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-Wformat=2",
            "-Wshadow",
            "-Wstrict-prototypes",
            "-Wmissing-prototypes",
            str(SOURCE),
        ]
        subprocess.run(common[:1] + ["-DUU_HELPER_TEST"] + common[1:] + ["-o", str(cls.test_binary)], check=True)
        subprocess.run(common + ["-o", str(cls.prod_binary)], check=True)

    @classmethod
    def tearDownClass(cls):
        if hasattr(cls, "build_dir"):
            cls.build_dir.cleanup()

    def make_fixture(self, cards=None):
        if cards is None:
            cards = [
                (0, "i915", [
                    ("HDMI-A-2", "disconnected"),
                    ("DP-1", "disconnected"),
                    ("HDMI-A-1", "disconnected"),
                ])
            ]
        temp = tempfile.TemporaryDirectory(prefix="uu-display-helper-case-")
        self.addCleanup(temp.cleanup)
        return SyntheticDrm(temp.name, cards)

    def run_helper(self, fixture, command):
        return subprocess.run(
            [str(self.test_binary), "--fixture", str(fixture.root), str(fixture.state), command],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=5,
            check=False,
        )

    def assert_failed_without_state(self, fixture):
        self.assertFalse((fixture.state / "state").exists())

    def test_ensure_reuses_only_owned_hdmi_and_release_restores_detection(self):
        fixture = self.make_fixture()

        ensured = self.run_helper(fixture, "ensure")
        self.assertEqual(ensured.returncode, 0, ensured.stderr)
        self.assertEqual(ensured.stdout, "card0-HDMI-A-1\n")
        self.assertEqual(fixture.get_status("card0-HDMI-A-1"), "connected")
        self.assertEqual(fixture.get_force("card0-HDMI-A-1"), "on")
        self.assertEqual((fixture.state / "state").read_text(), "uid=%d\nconnector=card0-HDMI-A-1\n" % os.getuid())

        # A newly connected physical DP output must neither replace nor be written by ensure.
        fixture.set_status("card0-DP-1", "connected")
        repeated = self.run_helper(fixture, "ensure")
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        self.assertEqual(repeated.stdout, "card0-HDMI-A-1\n")
        self.assertEqual(fixture.get_status("card0-DP-1"), "connected")
        self.assertEqual(fixture.get_status("card0-HDMI-A-2"), "disconnected")
        self.assertEqual(self.run_helper(fixture, "status").stdout, "card0-HDMI-A-1\n")

        released = self.run_helper(fixture, "release")
        self.assertEqual(released.returncode, 0, released.stderr)
        self.assertFalse((fixture.state / "state").exists())
        self.assertEqual(fixture.get_status("card0-HDMI-A-1"), "disconnected")
        self.assertEqual(fixture.get_force("card0-HDMI-A-1"), "unspecified")
        self.assertEqual(fixture.get_status("card0-DP-1"), "connected")

    def test_existing_force_override_is_never_replaced(self):
        fixture = self.make_fixture()
        fixture.set_force("card0-HDMI-A-1", "off")

        first_ensure = self.run_helper(fixture, "ensure")
        self.assertNotEqual(first_ensure.returncode, 0)
        self.assertEqual(fixture.get_status("card0-HDMI-A-1"), "disconnected")
        self.assertEqual(fixture.get_force("card0-HDMI-A-1"), "off")
        self.assert_failed_without_state(fixture)

        fixture.state.mkdir(mode=0o700, exist_ok=True)
        os.chmod(fixture.state, 0o700)
        (fixture.state / "state").write_text(
            f"uid={os.getuid()}\nconnector=card0-HDMI-A-1\n"
        )
        os.chmod(fixture.state / "state", 0o600)
        repeated = self.run_helper(fixture, "ensure")
        self.assertNotEqual(repeated.returncode, 0)
        released = self.run_helper(fixture, "release")
        self.assertNotEqual(released.returncode, 0)
        self.assertEqual(fixture.get_force("card0-HDMI-A-1"), "off")
        self.assertTrue((fixture.state / "state").exists())

    def test_already_connected_physical_output_is_a_safe_noop(self):
        fixture = self.make_fixture()
        fixture.set_status("card0-DP-1", "connected")

        result = self.run_helper(fixture, "ensure")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(fixture.get_status("card0-HDMI-A-1"), "disconnected")
        self.assert_failed_without_state(fixture)

    def test_unknown_connector_state_fails_closed(self):
        fixture = self.make_fixture()
        fixture.set_status("card0-DP-1", "unknown")

        result = self.run_helper(fixture, "ensure")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(fixture.get_status("card0-HDMI-A-1"), "disconnected")
        self.assert_failed_without_state(fixture)

    def test_requires_one_i915_card_and_an_existing_hdmi_connector(self):
        cases = [
            [(0, "amdgpu", [("HDMI-A-1", "disconnected")])],
            [
                (0, "i915", [("HDMI-A-1", "disconnected")]),
                (1, "i915", [("HDMI-A-1", "disconnected")]),
            ],
            [(0, "i915", [("DP-1", "disconnected")])],
        ]
        for cards in cases:
            with self.subTest(cards=cards):
                fixture = self.make_fixture(cards)
                result = self.run_helper(fixture, "ensure")
                self.assertNotEqual(result.returncode, 0)
                self.assert_failed_without_state(fixture)

    def test_root_boot_record_can_be_adopted_but_not_released_by_user(self):
        fixture = self.make_fixture()
        fixture.state.mkdir(mode=0o700, exist_ok=True)
        os.chmod(fixture.state, 0o700)
        (fixture.state / "state").write_text("uid=0\nconnector=card0-HDMI-A-1\n")
        os.chmod(fixture.state / "state", 0o600)
        fixture.set_status("card0-HDMI-A-1", "connected")
        fixture.set_force("card0-HDMI-A-1", "on")

        ensured = self.run_helper(fixture, "ensure")
        self.assertEqual(ensured.returncode, 0, ensured.stderr)
        self.assertEqual(ensured.stdout, "card0-HDMI-A-1\n")
        if os.getuid() != 0:
            released = self.run_helper(fixture, "release")
            self.assertNotEqual(released.returncode, 0)
            self.assertTrue((fixture.state / "state").exists())

    def test_untrusted_record_and_production_fixture_arguments_are_rejected(self):
        fixture = self.make_fixture()
        fixture.state.mkdir(mode=0o700, exist_ok=True)
        os.chmod(fixture.state, 0o700)
        different_uid = os.getuid() + 1
        if different_uid == 0:
            different_uid += 1
        (fixture.state / "state").write_text(
            f"uid={different_uid}\nconnector=card0-HDMI-A-1\n"
        )
        os.chmod(fixture.state / "state", 0o600)
        fixture.set_status("card0-HDMI-A-1", "connected")
        # Real records are created while holding a private lock; reconstruct that complete state
        # so this case reaches the caller-ownership check rather than failing on a missing lock.
        (fixture.state / "lock").write_text("")
        os.chmod(fixture.state / "lock", 0o600)
        result = self.run_helper(fixture, "status")
        if os.getuid() == 0:
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "card0-HDMI-A-1\n")
        else:
            self.assertNotEqual(result.returncode, 0)

        prod = subprocess.run(
            [str(self.prod_binary), "--fixture", "/sys", str(fixture.state), "ensure"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=5,
            check=False,
        )
        self.assertNotEqual(prod.returncode, 0)
        self.assertEqual(fixture.get_status("card0-HDMI-A-1"), "connected")


if __name__ == "__main__":
    unittest.main(verbosity=2)
