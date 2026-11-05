"""Check action immutability and keep release write access in one publish step."""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

class WorkflowTests(unittest.TestCase):
    def test_actions_have_full_commit_pins_and_version_comments(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            for line in path.read_text().splitlines():
                if re.match(r"\s*- uses:", line):
                    with self.subTest(workflow=path.name, step=line.strip()):
                        self.assertRegex(line, r"uses: [\w.-]+/[\w./-]+@[0-9a-f]{40} +# +\S+")

    def test_write_permission_is_limited_to_a_single_publish_step(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            text = path.read_text()
            self.assertRegex(text, r"(?m)^permissions:\n  contents: read$")
            jobs = re.split(r"(?m)^  [\w-]+:\n", text.split("jobs:\n", 1)[1])
            for job in jobs:
                if "contents: write" not in job:
                    continue
                with self.subTest(workflow=path.name):
                    steps = job.split("    steps:\n", 1)[1]
                    self.assertEqual(len(re.findall(r"(?m)^      - ", steps)), 1)
                    self.assertIn("- name: Publish the release", steps)
                    self.assertNotIn("uses:", steps)

if __name__ == "__main__":
    unittest.main()
