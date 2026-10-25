"""Unit tests for tools/drive.py. Run: python3 -m unittest tools/drive_test.py"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import drive  # noqa: E402


def dump(tabs, in_flight=0, quiet_ms=900):
    return {"tabs": tabs, "in_flight": in_flight, "quiet_ms": quiet_ms}


class SettleTest(unittest.TestCase):
    def test_settled_needs_idle_tabs_no_calls_and_quiet(self):
        idle = [{"busy": False}]
        self.assertTrue(drive.settled(dump(idle)))
        self.assertFalse(drive.settled(dump([{"busy": True}])))
        self.assertFalse(drive.settled(dump(idle, in_flight=1)))
        self.assertFalse(drive.settled(dump(idle, quiet_ms=499)))
        self.assertTrue(drive.settled(dump(idle, quiet_ms=500)))
        self.assertFalse(drive.settled(dump(idle, quiet_ms=None)))


class RedactTest(unittest.TestCase):
    def test_masks_secret_lines_and_the_username(self):
        lines = [
            "plain line",
            "got access_token abc",
            "Authorization: Bearer x",
            "auth failed",
            "hello someuser",
        ]
        out = drive.redact(lines, "someuser")
        self.assertEqual(out[0], "plain line")
        self.assertTrue(all(line == "[redacted]" for line in out[1:]))

    def test_no_username_masks_only_secret_words(self):
        self.assertEqual(drive.redact(["a b"], None), ["a b"])


class ConfigTest(unittest.TestCase):
    def test_renders_scalars_and_one_level_tables(self):
        text = drive.render_config(
            {"internal_api": False, "internal_api_areas": {"metadata": False}, "x": "y"}
        )
        self.assertIn("internal_api = false\n", text)
        self.assertIn('x = "y"\n', text)
        self.assertIn("[internal_api_areas]\nmetadata = false\n", text)
        # Tables come after top-level keys, as TOML requires.
        self.assertLess(text.index("x ="), text.index("[internal_api_areas]"))


class ExitCodeTest(unittest.TestCase):
    def test_maps_results_to_exit_codes(self):
        self.assertEqual(drive.exit_code(["PASS", "PASS"]), 0)
        self.assertEqual(drive.exit_code(["PASS", "FAIL", "BLOCKED"]), 1)
        self.assertEqual(drive.exit_code(["PASS", "BLOCKED"]), 3)
        self.assertEqual(drive.exit_code([]), 0)


class WriteBackTest(unittest.TestCase):
    OLD = b'{"access_token": "a"}'
    NEW = b'{"access_token": "b"}'

    def test_unchanged_temp_is_not_written(self):
        self.assertFalse(drive.should_write_back(self.OLD, self.OLD, self.OLD))

    def test_changed_temp_with_unchanged_real_is_written(self):
        self.assertTrue(drive.should_write_back(self.OLD, self.NEW, self.OLD))

    def test_real_changed_meanwhile_is_left_alone(self):
        self.assertFalse(drive.should_write_back(self.OLD, self.NEW, b'{"access_token": "c"}'))

    def test_unusable_temp_is_not_written(self):
        self.assertFalse(drive.should_write_back(self.OLD, None, self.OLD))
        self.assertFalse(drive.should_write_back(self.OLD, b"not json", self.OLD))
        self.assertFalse(drive.should_write_back(self.OLD, b'{"access_token": ""}', self.OLD))
        self.assertFalse(drive.should_write_back(self.OLD, b"{}", self.OLD))

    def test_missing_real_is_not_written(self):
        self.assertFalse(drive.should_write_back(self.OLD, self.NEW, None))


class TokenFileTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def path(self, name):
        return os.path.join(self.dir.name, name)

    def test_copy_returns_exactly_the_bytes_written(self):
        src, dst = self.path("real.json"), self.path("copy.json")
        with open(src, "wb") as f:
            f.write(b'{"access_token": "a"}')
        copied = drive.copy_token(src, dst)
        with open(dst, "rb") as f:
            self.assertEqual(f.read(), copied)
        self.assertEqual(copied, b'{"access_token": "a"}')
        self.assertEqual(os.stat(dst).st_mode & 0o777, 0o600)

    def test_atomic_write_replaces_the_file_private(self):
        real = self.path("real.json")
        with open(real, "wb") as f:
            f.write(b"old")
        drive.write_atomic(real, b"new")
        with open(real, "rb") as f:
            self.assertEqual(f.read(), b"new")
        self.assertEqual(os.stat(real).st_mode & 0o777, 0o600)
        self.assertFalse(os.path.exists(real + ".tmp"))

    def test_failed_atomic_write_leaves_no_part_file(self):
        real = self.path("real.json")
        os.mkdir(real)  # os.replace onto a directory fails
        with self.assertRaises(OSError):
            drive.write_atomic(real, b"new")
        self.assertFalse(os.path.exists(real + ".tmp"))


if __name__ == "__main__":
    unittest.main()
