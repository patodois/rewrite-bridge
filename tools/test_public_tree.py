"""Accepted text must be scanned completely, including above the small-blob cutoff."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

GATE = Path(__file__).with_name("check_public_tree.py")
MIB = 1024 * 1024

class PublicTreeTests(unittest.TestCase):
    def audit(self, data, config="", allow=""):
        with tempfile.TemporaryDirectory(prefix="public-tree-test-") as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "probe.txt").write_bytes(data)
            if config:
                (root / ".public-tree.cfg").write_text(config)
            if allow:
                (root / ".public-tree-allow").write_text(allow)
            subprocess.run(["git", "-C", str(root), "add", "."], check=True)
            result = subprocess.run([sys.executable, str(GATE), "--repo", str(root), "--json"], capture_output=True, text=True)
            self.assertIn(result.returncode, (0, 1), result.stderr)
            return json.loads(result.stdout), result.returncode

    def test_markers_at_the_start_boundary_and_end_of_accepted_text(self):
        marker = ("Decompiled " + "with CFR").encode()
        for size, offset in [(100,0),(MIB+1,0),(MIB+100,MIB-5),(2*MIB,2*MIB-len(marker))]:
            data = b" " * offset + marker
            data += b" " * (size-len(data))
            with self.subTest(size=size, offset=offset):
                result, code = self.audit(data)
                self.assertEqual(code, 1, "accepted text escaped content scanning")
                self.assertIn("decompiler-marker", str(result))

    def test_text_limit_and_explicit_larger_limits_are_scanned(self):
        marker = ("Decompiled " + "with CFR").encode()
        clean = b" " * (2*MIB)
        self.assertEqual(self.audit(clean)[1], 0)
        self.assertEqual(self.audit(clean+b" ")[1], 1)
        larger = b" " * (2*MIB+10) + marker
        for config, allow in [("max_text_bytes 3145728\n", ""), ("", "probe.txt max=3145728 # synthetic text fixture\n")]:
            with self.subTest(config=config, allow=allow):
                result, code = self.audit(larger, config, allow)
                self.assertEqual(code, 1)
                self.assertIn("decompiler-marker", str(result))

if __name__ == "__main__":
    unittest.main()
