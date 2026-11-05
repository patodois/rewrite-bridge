"""The interop judge must require real presentation checks and nonempty result objects."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

JUDGE = Path(__file__).with_name("check_results.py")


def reports():
    common = dict(mode="plain", ok=True, connected=True, handshake_failed=[], protocol=1,
                  session=7, failures=[], stats=None, frames_bad=0)
    host = dict(common, role="host", peer_game="interop guest (java)", frames_ok=5,
                presentation_rejections=9, poses=5, spawns=[7], despawns=[7],
                focus=["host:request"], cameras=0, collision_triangles=0, lost_reason="peer said bye")
    guest = dict(common, role="guest", peer_game="interop host (rust)", frames_ok=0,
                 presentation_rejections=0, poses=5, spawns=[3], despawns=[3],
                 focus=["guest:set", "host:set"], cameras=5, collision_triangles=300, lost_reason=None)
    return host, guest


class ResultsTests(unittest.TestCase):
    def judge(self, host, guest):
        with tempfile.TemporaryDirectory(prefix="interop-judge-") as directory:
            paths = [Path(directory) / "host.log", Path(directory) / "guest.log"]
            for path, value in zip(paths, (host, guest)):
                path.write_text("RBL-INTEROP-RESULT " + json.dumps(value) + "\n")
            return subprocess.run([sys.executable, str(JUDGE), "--host-label", "rust", "--guest-label", "java",
                                   str(paths[0]), str(paths[1])], capture_output=True, text=True)

    def test_valid_measured_session(self):
        result = self.judge(*reports())
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_presentation_checks_are_mandatory(self):
        host, guest = reports()
        for missing in (True, False):
            if missing:
                host.pop("presentation_rejections", None)
            else:
                host["presentation_rejections"] = 0
            with self.subTest(missing=missing):
                result = self.judge(host, guest)
                self.assertNotEqual(result.returncode, 0, "an unmeasured presentation path passed")

    def test_empty_or_nonobject_reports_do_not_pass(self):
        for value in ({}, [], None, False):
            with self.subTest(value=value):
                self.assertNotEqual(self.judge(value, value).returncode, 0)


if __name__ == "__main__":
    unittest.main()
